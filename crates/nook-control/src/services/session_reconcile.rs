//! The session reconciler (MAIN-316).
//!
//! A workspace declares what it wants ([`SessionSpec`], MAIN-315); nodes carry
//! the labels and taints that decide where it can go (MAIN-314). Nothing until
//! now closed the loop between them. This is the control loop: read desired,
//! read actual, converge.
//!
//! **Gated on `sessions.reconcile.enabled`, default off** — the same shape as
//! the loops switch, and for the same reason: the failure of "off by default"
//! is a workspace that waits until somebody notices, and the failure of "on by
//! default" is a fleet booting sessions nobody asked for.
//!
//! ## The marker, and why it is a column
//!
//! The reconciler may only ever touch sessions it created (AC-3). Deriving that
//! from workspace + runtime + eligible node was the alternative, and it is
//! **unsafe**: a session somebody hand-started in a managed workspace would be
//! counted as a replica and then killed when replicas dropped. So `sessions`
//! carries `managed`, and the reconciler's queries are scoped to it.
//!
//! MAIN-318 owns the rest of that concept — exposing it through the session
//! API, scale-down semantics, and what the UI offers. This card adds only the
//! column the loop cannot work without.
//!
//! ## One action wins, without a lease
//!
//! Every replica runs this. Two replicas seeing the same missing session both
//! try to start it, and the database decides: a partial unique index on
//! `checkout_id` over LIVE managed sessions means the loser's insert fails and
//! it moves on. No lease to expire, no lock to leak, no window where both
//! succeed. The same index is what makes "one managed session per checkout"
//! true rather than merely intended.
//!
//! ## Two levels: repo replicas, then per-worktree
//!
//! `replicas` decides how many NODES hold a clone of the repo (Single, All, or a
//! Count). Within each chosen node, EVERY present checkout — the clone and each
//! worktree — gets its own managed session. So the placement unit is the
//! checkout, not the node: a node holding a clone plus two worktrees runs three
//! managed sessions. Clone-on-demand ensures a chosen node that has no checkout
//! yet gets one so the next pass can place against it.

use std::collections::BTreeMap;
use std::time::Duration;

use nook_types::{
    NodeId, NodeWorkspaceId, Replicas, SessionId, SessionSpec, TenantId, WorkspaceId,
};

use crate::state::AppState;

/// How long between passes. Reconciling is a handful of indexed reads; the
/// interval is set by how fast a crashed session should come back, not by cost.
const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// The settings key. Tenant-scoped rows only, exactly like `loops.enabled` — a
/// `user`-scoped row of the same name is somebody's preference and must never
/// gate the fleet.
pub const KEY: &str = "sessions.reconcile.enabled";

/// Is reconciling enabled for this tenant? Absent → `false`.
///
/// Fails **closed** for the same reason the loops switch does: a transient
/// database error must not read as "start booting sessions".
pub async fn enabled(
    settings: &dyn crate::repo::admin::SettingRepository,
    tenant: TenantId,
) -> bool {
    truthy(
        settings
            .tenant_value(tenant, KEY)
            .await
            .unwrap_or(None)
            .as_ref(),
    )
}

/// Is ANY tenant reconciling? The cheap gate before doing a pass at all.
pub async fn any_enabled(settings: &dyn crate::repo::admin::SettingRepository) -> bool {
    settings
        .tenant_values_everywhere(KEY)
        .await
        .unwrap_or_default()
        .iter()
        .any(|v| truthy(Some(v)))
}

/// A stored value that means "on". Same tolerance as the loops switch: the
/// settings endpoint takes arbitrary JSON, so a hand-`PUT` string is possible.
/// Anything that is not an explicit true — including absent — is off.
fn truthy(v: Option<&serde_json::Value>) -> bool {
    match v {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true"),
        Some(serde_json::Value::Number(n)) => n.as_i64() == Some(1),
        _ => false,
    }
}

// ── the planner ─────────────────────────────────────────────────────────────

/// What the planner needs to know about one node. Assembled by the caller from
/// the node row and its checkouts; kept as plain data so the decision itself is
/// pure and testable without a database or a fleet.
#[derive(Debug, Clone)]
pub struct NodeFacts {
    pub id: NodeId,
    /// Offline nodes are not eligible — a session cannot be started on one, and
    /// counting it toward `replicas` would report a desired state that is not
    /// reachable.
    pub online: bool,
    pub labels: BTreeMap<String, String>,
    pub taints: Vec<nook_types::NodeTaint>,
    /// The runtimes this node can actually launch — `Capabilities.runtimes`,
    /// reported at register (`claude`/`codex`/`bash`/…). A spec's runtime must be
    /// in here or the node is ineligible: placing a `claude` session on a node
    /// with no claude binary would only fail at start. Empty means the node
    /// reported none (an older node) and is treated as unknown, not incapable —
    /// eligibility falls back to selector + taints alone.
    pub runtimes: Vec<String>,
}

/// One present checkout of the workspace — a clone OR a worktree — as a
/// placement slot. `replicas` decides how many NODES hold the repo; within each
/// chosen node, every one of these gets its own managed session (per-worktree).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutSlot {
    pub checkout_id: NodeWorkspaceId,
    pub node_id: NodeId,
    pub path: String,
}

/// A live managed session, as the planner sees it — keyed on the CHECKOUT it
/// occupies. The node rides along because stopping it means killing a process
/// on that machine, not only ending a row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actual {
    pub session_id: SessionId,
    pub checkout_id: NodeWorkspaceId,
    pub node_id: NodeId,
}

/// One thing to do. Deliberately not "kill" — see [`Plan::stop`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Start a managed session in this checkout (the session's cwd is `path`).
    Start {
        checkout: NodeWorkspaceId,
        node: NodeId,
        path: String,
    },
    /// Stop a managed session: its checkout is gone, or its node is no longer a
    /// chosen repo holder. Carries the node because stopping means killing the
    /// process there, not only ending the row.
    Stop { session: SessionId, node: NodeId },
}

/// The desired-vs-actual verdict for one workspace.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plan {
    pub actions: Vec<Action>,
    /// Nodes that match the selector and tolerate the taints but have no
    /// checkout yet (NG-1 — MAIN-317 clones them).
    pub needs_clone: Vec<NodeId>,
    /// How many sessions the spec asks for.
    pub desired: usize,
    /// How many are running or about to be.
    pub placed: usize,
    /// `desired - placed`: asked for more than the fleet can host. Reported,
    /// not corrected — stacking two sessions on one node is not the answer.
    pub shortfall: usize,
}

impl Plan {
    fn stop(&mut self, a: &Actual) {
        self.actions.push(Action::Stop {
            session: a.session_id,
            node: a.node_id,
        });
    }
}

/// Does this node satisfy the spec's selector and taints?
///
/// Selector is subset-match: every declared `key=value` must be present with
/// that value. An empty selector matches every node, which is what "I do not
/// care where" should mean.
fn eligible(spec: &SessionSpec, node: &NodeFacts) -> bool {
    if !node.online {
        return false;
    }
    // The runtime has to be installable here. A node that reported no runtimes is
    // unknown, not incapable — don't exclude it on missing data (an older node
    // still places under selector + taints). But once we DO know its runtimes, a
    // spec asking for one it lacks is a refusal: the session would only fail at
    // start on that machine.
    if !node.runtimes.is_empty() && !node.runtimes.contains(&spec.runtime) {
        return false;
    }
    if !spec
        .node_selector
        .iter()
        .all(|(k, v)| node.labels.get(k) == Some(v))
    {
        return false;
    }
    // Every taint must be tolerated — one untolerated taint is a refusal, no
    // matter how well the labels match. Key AND effect, because tolerating
    // `gpu:NoSchedule` says nothing about `gpu` under some future effect.
    node.taints.iter().all(|t| {
        spec.tolerations
            .iter()
            .any(|tol| tol.key == t.key && tol.effect == t.effect)
    })
}

/// Desired, actual, and the difference — for ONE workspace.
///
/// Pure, and deterministic: nodes are considered in id order, so two replicas
/// planning the same instant produce the same plan. That is not cosmetic — it
/// is what stops replica A starting on node 1 while replica B starts on node 2
/// for the same single-replica spec.
pub fn plan(
    spec: &SessionSpec,
    nodes: &[NodeFacts],
    checkouts: &[CheckoutSlot],
    actual: &[Actual],
) -> Plan {
    let has_checkout = |id: NodeId| checkouts.iter().any(|c| c.node_id == id);
    // Eligible nodes, checkout-holders first so a bounded replica count prefers
    // an existing clone over cloning a fresh one; stable by id within a group.
    let mut eligible_nodes: Vec<&NodeFacts> = nodes.iter().filter(|n| eligible(spec, n)).collect();
    eligible_nodes.sort_by_key(|n| (!has_checkout(n.id), n.id.0));

    // How many NODES should hold the repo — the "repo replicas".
    let target_nodes = match spec.replicas {
        Replicas::Count { count } => count as usize,
        Replicas::Single => 1,
        Replicas::All => eligible_nodes.len(),
    };
    let chosen: Vec<NodeId> = eligible_nodes
        .iter()
        .take(target_nodes)
        .map(|n| n.id)
        .collect();
    // Replica slots with no node to land on (e.g. Count{5} with 2 eligible) —
    // pure shortfall, nothing to clone onto.
    let unmet_nodes = target_nodes.saturating_sub(eligible_nodes.len());

    // The placement slots: every present checkout on a chosen node.
    let mut slots: Vec<&CheckoutSlot> = checkouts
        .iter()
        .filter(|c| chosen.contains(&c.node_id))
        .collect();
    slots.sort_by_key(|c| c.checkout_id.0);

    // A chosen node with NO checkout wants a clone — one pending slot each.
    let needs_clone: Vec<NodeId> = chosen
        .iter()
        .copied()
        .filter(|id| !has_checkout(*id))
        .collect();

    let mut out = Plan {
        needs_clone: needs_clone.clone(),
        ..Default::default()
    };

    // Keep sessions whose checkout is still a slot; stop the rest — its checkout
    // is gone, or its node is no longer a chosen repo holder.
    let slot_ids: Vec<NodeWorkspaceId> = slots.iter().map(|c| c.checkout_id).collect();
    let mut held: Vec<NodeWorkspaceId> = Vec::new();
    for a in actual {
        if slot_ids.contains(&a.checkout_id) {
            held.push(a.checkout_id);
        } else {
            out.stop(a);
        }
    }

    // Start a session for every slot not already held.
    for c in &slots {
        if !held.contains(&c.checkout_id) {
            out.actions.push(Action::Start {
                checkout: c.checkout_id,
                node: c.node_id,
                path: c.path.clone(),
            });
        }
    }

    // Sessions land on real checkouts; the shortfall is the pending clones plus
    // replica slots that had no eligible node at all.
    out.placed = slots.len();
    out.desired = slots.len() + needs_clone.len() + unmet_nodes;
    out.shortfall = needs_clone.len() + unmet_nodes;
    out
}

// ── the loop ────────────────────────────────────────────────────────────────

/// Log the switch only when it CHANGES, so a 10-second poll does not fill the
/// log forever. `None` is "never reported", which is why the first tick always
/// speaks: an operator needs the state they booted into.
#[derive(Default)]
pub struct SwitchLog {
    last: Option<bool>,
}

impl SwitchLog {
    pub fn observe(&mut self, on: bool) -> bool {
        if self.last != Some(on) {
            if on {
                tracing::info!("session reconcile enabled — converging");
            } else {
                tracing::info!(
                    "session reconcile disabled — idle. Enable with the \
                     `sessions.reconcile.enabled` tenant setting."
                );
            }
            self.last = Some(on);
        }
        on
    }
}

/// Spawn the reconciler. Fire-and-forget, like `job_dispatch`: the process
/// exits on shutdown, taking the task with it.
pub fn start(state: AppState) {
    tokio::spawn(async move {
        tracing::info!("session reconciler started");
        run(state).await;
    });
}

/// Remembers when a clone was last asked of a (workspace, node), so a repo that
/// cannot be cloned — a private URL with no credential, a path that 404s — is
/// retried at most once per [`CLONE_RETRY_TTL`] instead of on every 10s pass.
/// In-process and per-replica: two replicas may each issue one initial clone,
/// which git handles (the second lands on an existing/in-progress directory and
/// fails harmlessly). It only ever grows by (workspace, node) pairs the fleet
/// actually has, so it needs no eviction.
#[derive(Default)]
pub struct CloneThrottle {
    last: std::collections::HashMap<(WorkspaceId, NodeId), std::time::Instant>,
}

const CLONE_RETRY_TTL: Duration = Duration::from_secs(60);

impl CloneThrottle {
    /// True at most once per TTL per pair; records the moment it says yes.
    fn may_issue(&mut self, workspace: WorkspaceId, node: NodeId) -> bool {
        let now = std::time::Instant::now();
        match self.last.get(&(workspace, node)) {
            Some(at) if now.duration_since(*at) < CLONE_RETRY_TTL => false,
            _ => {
                self.last.insert((workspace, node), now);
                true
            }
        }
    }
}

async fn run(state: AppState) {
    let mut switch = SwitchLog::default();
    let mut clones = CloneThrottle::default();
    loop {
        // Re-read every tick, so a flip lands within one interval with no
        // restart. With every tenant off this is one indexed lookup and the
        // pass ends — "off" is quiet, not merely ineffective.
        if !switch.observe(any_enabled(&*state.settings).await) {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }
        if let Err(e) = pass(&state, &mut clones).await {
            // A failed pass is not fatal: the next one re-reads the world from
            // scratch, which is the point of a reconciler over a queue.
            tracing::warn!(error = %e, "session reconcile pass failed");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// The spec a workspace reconciles toward when its tenant is on but it carries
/// no explicit `session_spec`. This is what makes reconcile a fleet-wide switch
/// rather than a per-workspace opt-in: flipping it on means "the state of the
/// world is a live terminal wherever a workspace is checked out." A bash session
/// on every eligible node that has the checkout (`Replicas::All`).
///
/// Per-node today — one managed session per (workspace, node-with-the-clone).
/// Per-worktree (one per checkout, worktrees included) is the follow-up; the
/// planner keys on nodes, not checkouts, until then.
///
/// `pub(crate)` so the reconcile-status endpoint derives the spec the SAME way
/// the loop does — a status that read "unmanaged" for a workspace the loop is
/// about to give a default session to is exactly the drift `node_facts`'
/// doc-comment warns against.
pub(crate) fn default_spec() -> SessionSpec {
    SessionSpec {
        runtime: "bash".into(),
        node_selector: Default::default(),
        tolerations: vec![],
        replicas: Replicas::All,
    }
}

/// One pass over every workspace in a reconcile-on tenant.
///
/// An explicit `session_spec` still wins — a workspace can ask for `claude`, a
/// selector, or zero replicas. Everything else in an on tenant gets
/// [`default_spec`], which is the auto-derive: no per-workspace opt-in, the
/// tenant switch is the whole gate.
async fn pass(state: &AppState, clones: &mut CloneThrottle) -> crate::error::ApiResult<()> {
    // Explicit specs, keyed by workspace so the per-workspace lookup below is a
    // map hit and not a query. Ids are uuids, unique across tenants, so a flat
    // map is safe even though this crosses every tenant.
    let explicit: std::collections::HashMap<WorkspaceId, SessionSpec> = managed_workspaces(state)
        .await?
        .into_iter()
        .map(|(_, w, s)| (w, s))
        .collect();

    for (tenant, value) in state.settings.tenants_with_value(KEY).await? {
        // The stored value is arbitrary JSON; a row can exist reading `false`.
        // Same truthiness as the per-tenant gate, so on/off agree everywhere.
        if !truthy(Some(&value)) {
            continue;
        }
        for ws in state.workspaces.list(tenant).await? {
            let spec = explicit.get(&ws.id).cloned().unwrap_or_else(default_spec);
            if let Err(e) = reconcile_workspace(state, tenant, ws.id, &spec, clones).await {
                // Per workspace, so one broken declaration does not stop the fleet.
                tracing::warn!(workspace = %ws.id, error = %e, "workspace reconcile failed");
            }
        }
    }
    Ok(())
}

/// Every workspace with a `session_spec`, and the spec parsed. A spec that does
/// not parse is skipped loudly rather than silently: it means somebody stored a
/// shape this build does not understand, and guessing at it would converge to
/// something nobody declared.
async fn managed_workspaces(
    state: &AppState,
) -> crate::error::ApiResult<Vec<(TenantId, WorkspaceId, SessionSpec)>> {
    let mut out = Vec::new();
    for (tenant, id, raw) in state.workspaces.all_session_specs().await? {
        match serde_json::from_value::<SessionSpec>(raw) {
            Ok(spec) => out.push((tenant, id, spec)),
            Err(e) => {
                tracing::warn!(workspace = %id, error = %e, "unreadable SessionSpec — skipped")
            }
        }
    }
    Ok(out)
}

async fn reconcile_workspace(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    spec: &SessionSpec,
    clones: &mut CloneThrottle,
) -> crate::error::ApiResult<()> {
    let nodes = node_facts(state, tenant).await?;
    let checkouts: Vec<CheckoutSlot> = state
        .workspaces
        .present_checkouts(tenant, workspace)
        .await?
        .into_iter()
        .map(|c| CheckoutSlot {
            checkout_id: c.id,
            node_id: c.node_id,
            path: c.path,
        })
        .collect();
    let actual = state.sessions.live_managed(tenant, workspace).await?;
    let actual: Vec<Actual> = actual
        .into_iter()
        .map(|(session_id, checkout_id, node_id)| Actual {
            session_id,
            checkout_id,
            node_id,
        })
        .collect();

    let plan = plan(spec, &nodes, &checkouts, &actual);

    // AC-4's "logs desired-vs-actual". One line per workspace per pass, and
    // only when there is something to say — a converged workspace is silent, or
    // a fleet at rest would be the noisiest thing in the log.
    if !plan.actions.is_empty() || plan.shortfall > 0 || !plan.needs_clone.is_empty() {
        tracing::info!(
            %workspace,
            desired = plan.desired,
            actual = actual.len(),
            starting = plan.actions.iter().filter(|a| matches!(a, Action::Start { .. })).count(),
            stopping = plan.actions.iter().filter(|a| matches!(a, Action::Stop { .. })).count(),
            shortfall = plan.shortfall,
            needs_clone = plan.needs_clone.len(),
            "reconciling workspace"
        );
    }

    for action in &plan.actions {
        match action {
            Action::Start {
                checkout,
                node,
                path,
            } => {
                // Losing the race is the NORMAL outcome on a multi-replica
                // deployment — the unique index means the other replica already
                // started it. Debug, not warn: it is the mechanism working.
                if let Err(e) = start_managed(state, tenant, workspace, *node, path, spec).await {
                    tracing::debug!(%workspace, node = %node, checkout = %checkout, error = %e, "managed start did not win");
                }
            }
            Action::Stop { session, node } => {
                // Kill FIRST, and only mark the row ended if the node took it.
                //
                // Ending the row alone was a defect, not a shortcut: the tmux
                // session keeps running, the reconciler can no longer see it —
                // `live_managed` reads live rows — and the freed index slot lets
                // the very next pass start a SECOND session on that machine. The
                // scale-down would have doubled the thing it was scaling down.
                //
                // A node that is offline keeps its row live, so the next pass
                // tries again. That is the honest state: the process is still
                // out there, and the row saying so is what will eventually stop
                // it.
                if !state.registry.send_to_node(
                    *node,
                    nook_proto::ControlToNode::KillSession {
                        session_id: *session,
                    },
                ) {
                    tracing::warn!(
                        %workspace, session = %session, node = %node,
                        "cannot stop a managed session — node offline; retrying next pass"
                    );
                    continue;
                }
                if let Err(e) = state.sessions.mark_ended(tenant, *session).await {
                    tracing::warn!(%workspace, session = %session, error = %e, "managed stop failed");
                }
            }
        }
    }

    // Clone-on-demand (MAIN-317): an eligible node that matched the spec but has
    // no checkout is no longer just a reported shortfall — we clone the workspace
    // onto it so the NEXT pass can place a session there. Throttled, because a
    // repo that cannot be cloned would otherwise be re-issued every pass.
    for node in &plan.needs_clone {
        if clones.may_issue(workspace, *node) {
            start_clone(state, tenant, workspace, *node).await;
        }
    }
    Ok(())
}

/// Ask a node to clone the workspace's repo. Fire-and-forget: discovery lands
/// the `node_workspaces` row and the next pass places the session, so holding
/// the pass on a multi-minute clone would only stall every other workspace.
///
/// No credential: the reconciler clones with the node's own reach — a local
/// path, or a public/preconfigured remote. A private repo that needs a secret
/// stays `needs_clone` rather than failing here; wiring the workspace's stored
/// credential in is the follow-up.
async fn start_clone(state: &AppState, tenant: TenantId, workspace: WorkspaceId, node: NodeId) {
    let url = match state.workspaces.git_remote_url(workspace, tenant).await {
        Ok(Some(Some(url))) => url,
        // No remote to clone from, or the workspace vanished — nothing to do.
        _ => {
            tracing::warn!(%workspace, node = %node, "clone-on-demand: no git remote to clone");
            return;
        }
    };
    let Some(rx) =
        state
            .registry
            .request_op(node, |request_id| nook_proto::ControlToNode::CloneRepo {
                request_id,
                url: url.clone(),
                dest_name: None,
                ssh_key: None,
            })
    else {
        // Went offline between planning and issuing; the next pass re-reads.
        return;
    };
    tracing::info!(%workspace, node = %node, url = %url, "clone-on-demand: cloning workspace onto node");
    // Log the outcome so a repeatedly-failing clone is visible rather than a
    // silent perpetual shortfall — but do NOT block the pass on it.
    let state = state.clone();
    tokio::spawn(async move {
        match tokio::time::timeout(std::time::Duration::from_secs(900), rx).await {
            Ok(Ok(p)) if p.ok => {
                // Pin the checkout row NOW instead of waiting for the node's next
                // discovery scan — otherwise the session can't place until the
                // node happens to re-report, which may be only on reconnect.
                // `associate_clone` is idempotent (ON CONFLICT), so a later scan
                // reporting the same path converges rather than duplicates.
                match p.path {
                    Some(path) => {
                        let normalized = crate::services::discovery::normalize_remote(&url);
                        match state
                            .workspaces
                            .associate_clone(tenant, node, workspace, &path, &url, &normalized)
                            .await
                        {
                            Ok(()) => {
                                tracing::info!(%workspace, node = %node, %path, "clone-on-demand: cloned and recorded checkout")
                            }
                            Err(e) => {
                                tracing::warn!(%workspace, node = %node, error = %e, "clone-on-demand: cloned but could not record checkout")
                            }
                        }
                    }
                    None => {
                        tracing::warn!(%workspace, node = %node, "clone-on-demand: clone ok but node returned no path")
                    }
                }
            }
            Ok(Ok(p)) => {
                tracing::warn!(%workspace, node = %node, message = %p.message, "clone-on-demand: clone failed")
            }
            Ok(Err(_)) => {
                tracing::warn!(%workspace, node = %node, "clone-on-demand: node disconnected mid-clone")
            }
            Err(_) => tracing::warn!(%workspace, node = %node, "clone-on-demand: clone timed out"),
        }
    });
}

/// Eligibility facts for every node in the tenant — online, labels, taints,
/// runtimes. Workspace-independent now: which checkouts a workspace has is a
/// separate read (`present_checkouts`), because placement is per-checkout.
///
/// `pub(crate)` so the status endpoint (MAIN-319) reads the world exactly as
/// the loop does. A second gatherer would drift, and the first symptom would be
/// a UI confidently reporting a placement the reconciler does not agree with.
pub(crate) async fn node_facts(
    state: &AppState,
    tenant: TenantId,
) -> crate::error::ApiResult<Vec<NodeFacts>> {
    let mut out = Vec::new();
    // `None` owner: the reconciler places across the whole tenant fleet, not one
    // person's machines.
    // Home-tenant only: reconcile places managed sessions for a tenant's own
    // workspaces (MAIN-353 NG-3).
    for node in state.nodes.list(tenant, None, None).await? {
        let placement = crate::routes::nodes::placement_of(&node);
        // Runtimes are stored as `capabilities.runtimes` (a jsonb array) — the
        // same value the node reported at register. Absent/malformed reads as an
        // empty list, which `eligible` treats as "unknown", not "none".
        let runtimes = node
            .capabilities
            .get("runtimes")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        out.push(NodeFacts {
            id: node.id,
            online: state.registry.node_online(node.id),
            labels: placement.labels,
            taints: placement.taints,
            runtimes,
        });
    }
    Ok(out)
}

/// Start a managed session in a specific checkout. The `path` is that checkout's
/// working directory — `create_session_at` re-resolves `checkout_id` from it, so
/// the session binds to the exact clone or worktree the planner chose.
async fn start_managed(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    node: NodeId,
    path: &str,
    spec: &SessionSpec,
) -> crate::error::ApiResult<()> {
    // `managed: true` on the INSERT is the whole race arbitration. It used to be
    // a follow-up UPDATE, which meant the losing replica had ALREADY inserted an
    // ad-hoc row and sent `StartSession` — a live session nothing would ever
    // reconcile. Now the index refuses the row, `create_session_at` returns
    // before it talks to the node, and the loser really does just lose.
    crate::services::session_queries::create_session_at(
        state,
        tenant,
        // No creator: the control plane declared this, not a person. MAIN-318
        // is where the UI learns to say so.
        None,
        workspace,
        node,
        &spec.runtime,
        Some(format!("{} (managed)", spec.runtime)),
        path,
        true,
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use nook_types::{NodeTaint, Toleration};
    use uuid::Uuid;

    fn node(n: u8, labels: &[(&str, &str)], taints: &[(&str, &str)]) -> NodeFacts {
        NodeFacts {
            id: NodeId(Uuid::from_u128(n as u128)),
            online: true,
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            taints: taints
                .iter()
                .map(|(k, e)| NodeTaint {
                    key: k.to_string(),
                    effect: e.to_string(),
                })
                .collect(),
            // Empty = "runtimes unknown", which `eligible` treats as no
            // constraint. The runtime-specific test below sets it explicitly.
            runtimes: vec![],
        }
    }

    /// A checkout with id `cid` on node `on`. `co(n)` is the clone on node n
    /// (one checkout per node, the pre-per-worktree shape); a second checkout on
    /// the same node stands in for a worktree.
    fn checkout(cid: u8, on: u8) -> CheckoutSlot {
        CheckoutSlot {
            checkout_id: NodeWorkspaceId(Uuid::from_u128(2000 + cid as u128)),
            node_id: NodeId(Uuid::from_u128(on as u128)),
            path: format!("/w/{cid}"),
        }
    }
    fn co(n: u8) -> CheckoutSlot {
        checkout(n, n)
    }

    fn spec(replicas: Replicas) -> SessionSpec {
        SessionSpec {
            runtime: "claude".into(),
            node_selector: Default::default(),
            tolerations: vec![],
            replicas,
        }
    }

    fn starts(p: &Plan) -> Vec<NodeId> {
        p.actions
            .iter()
            .filter_map(|a| match a {
                Action::Start { node, .. } => Some(*node),
                _ => None,
            })
            .collect()
    }

    fn start_checkouts(p: &Plan) -> Vec<NodeWorkspaceId> {
        p.actions
            .iter()
            .filter_map(|a| match a {
                Action::Start { checkout, .. } => Some(*checkout),
                _ => None,
            })
            .collect()
    }

    fn stops(p: &Plan) -> Vec<SessionId> {
        p.actions
            .iter()
            .filter_map(|a| match a {
                Action::Stop { session, .. } => Some(*session),
                _ => None,
            })
            .collect()
    }

    fn running(session: u8, cid: u8, on: u8) -> Actual {
        Actual {
            session_id: SessionId(Uuid::from_u128(1000 + session as u128)),
            checkout_id: NodeWorkspaceId(Uuid::from_u128(2000 + cid as u128)),
            node_id: NodeId(Uuid::from_u128(on as u128)),
        }
    }

    #[test]
    fn a_node_missing_the_runtime_is_refused_but_unknown_runtimes_still_place() {
        // Placing a `claude` session on a node with no claude binary would only
        // fail at start, so a node reporting its runtimes and lacking the asked
        // one is refused. A node that reported NO runtimes is unknown, not
        // incapable — it still places, so an older node is not silently dropped.
        let mut has = node(1, &[], &[]);
        has.runtimes = vec!["bash".into(), "claude".into()];
        let mut lacks = node(2, &[], &[]);
        lacks.runtimes = vec!["bash".into(), "codex".into()];
        let unknown = node(3, &[], &[]); // runtimes: vec![] — unknown

        // `spec()` asks for "claude". Each node holds its clone.
        let p = plan(
            &spec(Replicas::All),
            &[has.clone(), lacks, unknown.clone()],
            &[co(1), co(2), co(3)],
            &[],
        );
        let started = starts(&p);
        assert!(started.contains(&has.id), "the claude-capable node places");
        assert!(
            started.contains(&unknown.id),
            "the unknown-runtime node still places"
        );
        assert_eq!(started.len(), 2, "the codex-only node is refused");
    }

    #[test]
    fn per_worktree_one_session_per_checkout_on_a_chosen_node() {
        // The point of the whole slice: a node holding a clone AND a worktree
        // gets a session for EACH, not one for the node. `replicas` is about how
        // many NODES hold the repo; within a node it is one per checkout.
        let nodes = [node(1, &[], &[])];
        let checkouts = [checkout(10, 1), checkout(11, 1)]; // clone + worktree on node 1
        let p = plan(&spec(Replicas::Single), &nodes, &checkouts, &[]);
        assert_eq!(start_checkouts(&p).len(), 2, "one session per checkout");
        assert_eq!(starts(&p), vec![nodes[0].id, nodes[0].id], "both on node 1");
        assert_eq!(p.desired, 2);
        assert_eq!(p.shortfall, 0);
    }

    #[test]
    fn an_empty_selector_matches_every_node() {
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let p = plan(&spec(Replicas::All), &nodes, &[co(1), co(2)], &[]);
        assert_eq!(starts(&p).len(), 2);
        assert_eq!(p.shortfall, 0);
    }

    #[test]
    fn a_selector_must_match_every_declared_pair() {
        let mut s = spec(Replicas::All);
        s.node_selector = [("os".into(), "linux".into()), ("gpu".into(), "yes".into())]
            .into_iter()
            .collect();
        let nodes = [
            node(1, &[("os", "linux")], &[]),                 // missing gpu
            node(2, &[("os", "linux"), ("gpu", "yes")], &[]), // both
            node(3, &[("os", "macos"), ("gpu", "yes")], &[]), // wrong os
        ];
        assert_eq!(
            starts(&plan(&s, &nodes, &[co(1), co(2), co(3)], &[])),
            vec![nodes[1].id]
        );
    }

    #[test]
    fn an_untolerated_taint_refuses_however_well_the_labels_match() {
        let s = spec(Replicas::All);
        let nodes = [node(1, &[], &[("no-loops", "NoSchedule")])];
        let p = plan(&s, &nodes, &[co(1)], &[]);
        assert!(starts(&p).is_empty());
        assert_eq!(p.desired, 0, "an ineligible node is not a desired slot");
    }

    #[test]
    fn a_toleration_must_match_the_effect_too() {
        // Tolerating `gpu:NoSchedule` says nothing about `gpu` under another
        // effect, and treating key alone as enough would place work on a node
        // that refused it for a different reason.
        let mut s = spec(Replicas::All);
        s.tolerations = vec![Toleration {
            key: "gpu".into(),
            effect: "NoSchedule".into(),
        }];
        let tolerated = [node(1, &[], &[("gpu", "NoSchedule")])];
        let other_effect = [node(2, &[], &[("gpu", "NoExecute")])];
        assert_eq!(starts(&plan(&s, &tolerated, &[co(1)], &[])).len(), 1);
        assert!(starts(&plan(&s, &other_effect, &[co(2)], &[])).is_empty());
    }

    #[test]
    fn an_offline_node_is_not_a_placement() {
        let mut n = node(1, &[], &[]);
        n.online = false;
        let p = plan(&spec(Replicas::Single), &[n], &[co(1)], &[]);
        assert!(starts(&p).is_empty());
        assert_eq!(p.shortfall, 1, "the desired session is still owed");
    }

    #[test]
    fn replicas_spread_one_per_node_and_never_double_up() {
        // AC-4: `replicas > eligible` places what it can and reports the rest.
        // Stacking two on one machine would satisfy the number and miss the
        // point of a spread. Each node holds one checkout, so this is one
        // session per node.
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let p = plan(
            &spec(Replicas::Count { count: 5 }),
            &nodes,
            &[co(1), co(2)],
            &[],
        );
        assert_eq!(starts(&p).len(), 2);
        assert_eq!(p.desired, 5);
        assert_eq!(p.placed, 2);
        assert_eq!(p.shortfall, 3);
    }

    #[test]
    fn a_node_without_the_checkout_needs_a_clone_rather_than_a_session() {
        // A chosen node with no checkout is reported AND counted as a shortfall —
        // clone-on-demand will clone it, but until then the workspace is not
        // getting what it asked for. Only node 1 has a checkout here.
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let p = plan(&spec(Replicas::Count { count: 2 }), &nodes, &[co(1)], &[]);
        assert_eq!(starts(&p).len(), 1);
        assert_eq!(p.needs_clone, vec![NodeId(Uuid::from_u128(2))]);
        assert_eq!(p.shortfall, 1);
    }

    #[test]
    fn all_counts_nodes_awaiting_a_clone_as_shortfall_not_as_a_lower_target() {
        // `All` means every eligible node holds the repo. If a chosen node has no
        // checkout, the target does not quietly shrink to hide it.
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let p = plan(&spec(Replicas::All), &nodes, &[co(1)], &[]);
        assert_eq!(p.desired, 2);
        assert_eq!(p.placed, 1);
        assert_eq!(p.shortfall, 1);
    }

    #[test]
    fn a_converged_workspace_does_nothing() {
        // Idempotence, which is the whole contract of a reconciler: running it
        // twice must not do anything the first run did not.
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let actual = [running(1, 1, 1), running(2, 2, 2)];
        let p = plan(
            &spec(Replicas::Count { count: 2 }),
            &nodes,
            &[co(1), co(2)],
            &actual,
        );
        assert!(p.actions.is_empty(), "{:?}", p.actions);
        assert_eq!(p.shortfall, 0);
    }

    #[test]
    fn a_crashed_session_is_replaced() {
        // The reconciler is handed only LIVE managed sessions, so a crash is
        // simply an actual that is no longer there — and the gap is filled. The
        // session on checkout 2 is gone, so it is restarted.
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let p = plan(
            &spec(Replicas::Count { count: 2 }),
            &nodes,
            &[co(1), co(2)],
            &[running(1, 1, 1)],
        );
        assert_eq!(starts(&p), vec![NodeId(Uuid::from_u128(2))]);
    }

    #[test]
    fn dropping_replicas_stops_the_surplus() {
        // Single keeps ONE repo-holder node (node 1, lowest id); the sessions on
        // the other two nodes' checkouts are stopped.
        let nodes = [node(1, &[], &[]), node(2, &[], &[]), node(3, &[], &[])];
        let actual = [running(1, 1, 1), running(2, 2, 2), running(3, 3, 3)];
        let p = plan(
            &spec(Replicas::Single),
            &nodes,
            &[co(1), co(2), co(3)],
            &actual,
        );
        assert!(starts(&p).is_empty());
        // Node order decides the victims, so every replica picks the same two.
        assert_eq!(stops(&p).len(), 2);
        assert!(
            !stops(&p).contains(&actual[0].session_id),
            "the first is kept"
        );
    }

    #[test]
    fn a_session_on_a_node_that_stopped_matching_is_stopped() {
        // The declaration moved — a node relabelled out of the selector, or
        // freshly tainted. The session has to follow the declaration.
        let mut s = spec(Replicas::Single);
        s.node_selector = [("os".into(), "linux".into())].into_iter().collect();
        let nodes = [node(1, &[("os", "macos")], &[])];
        let p = plan(&s, &nodes, &[co(1)], &[running(1, 1, 1)]);
        assert_eq!(stops(&p).len(), 1);
        assert_eq!(p.shortfall, 1, "and the workspace is now owed one");
    }

    #[test]
    fn planning_is_deterministic_whatever_order_the_nodes_arrive_in() {
        // Two replicas read the node list in whatever order their query
        // returned. If the plan depended on that, replica A would start on
        // node 1 while replica B started on node 2 for a single-replica spec.
        let a = [node(3, &[], &[]), node(1, &[], &[]), node(2, &[], &[])];
        let b = [node(1, &[], &[]), node(2, &[], &[]), node(3, &[], &[])];
        let cos = [co(1), co(2), co(3)];
        assert_eq!(
            starts(&plan(&spec(Replicas::Single), &a, &cos, &[])),
            starts(&plan(&spec(Replicas::Single), &b, &cos, &[]))
        );
    }

    #[test]
    fn zero_replicas_stops_everything_and_starts_nothing() {
        // MAIN-315 made "managed, wanting none" expressible on purpose. It has
        // to mean something here, and what it means is: stop them.
        let nodes = [node(1, &[], &[])];
        let p = plan(
            &spec(Replicas::Count { count: 0 }),
            &nodes,
            &[co(1)],
            &[running(1, 1, 1)],
        );
        assert!(starts(&p).is_empty());
        assert_eq!(stops(&p).len(), 1);
    }

    #[test]
    fn absent_is_off_and_that_is_the_whole_point() {
        assert!(!truthy(None));
        assert!(!truthy(Some(&serde_json::json!(false))));
        assert!(truthy(Some(&serde_json::json!(true))));
        assert!(truthy(Some(&serde_json::json!("true"))));
    }
}
