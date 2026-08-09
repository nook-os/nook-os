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
//!
//! ## Two declarations, one engine (MAIN-326)
//!
//! A workspace's own spec says what a PERSON can attach to. The control plane
//! has a second thing it wants of every repo — an always-on review loop on the
//! deployment's loop node — and that is [`review_loop_spec`]: selected onto
//! `role=loop`, running `nook-review` forever. Its shape is this build's and
//! not a caller's; only its CEILING is a workspace declaration (MAIN-445), read
//! from `review_loop_max_replicas` on every pass.
//!
//! It is the same planner, the same clone-on-demand and the same self-healing;
//! the only additions are which checkouts it may use ([`Slots`]), how many
//! sessions may share a node ([`Spread`]), and a [`ManagedPurpose`] on the
//! session so the two declarations cannot adopt or stop each other's work. Ticket-triggered spec/build jobs are a different
//! mechanism entirely (`job_dispatch`) and are untouched by any of this.

use std::collections::BTreeMap;
use std::time::Duration;

use nook_types::{
    ManagedPurpose, NodeId, NodeWorkspaceId, Replicas, SessionId, SessionSpec, TenantId,
    WorkspaceId,
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
    /// How many loop jobs this node holds at once — `capabilities.max_loop_jobs`,
    /// the number an operator already sets with `NOOK_MAX_LOOP_JOBS`.
    ///
    /// It is the ceiling on sharded placement (MAIN-446 AC-1), and it is the
    /// node's EXISTING number rather than a second one: a review loop is agent
    /// work on that machine exactly as a loop job is, and two answers to "how
    /// much agent work does this box run" would immediately disagree.
    ///
    /// `None` is an unreported cap (an older agent), which reads as
    /// [`jobs::CAPACITY_WHEN_UNREPORTED`] for the reason given there — the
    /// shipped default, not permission to take everything.
    pub max_loop_jobs: Option<u32>,
}

impl NodeFacts {
    /// How many managed sessions of one declaration this node may hold.
    fn capacity(&self) -> usize {
        self.max_loop_jobs
            .unwrap_or(crate::services::jobs::CAPACITY_WHEN_UNREPORTED) as usize
    }
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
    /// The slice of the declaration this session was placed to own (MAIN-446),
    /// read off its row. `SOLO` for every unsharded session, which is what an
    /// access session and a single reviewer both are.
    ///
    /// Part of the identity of a placement, not decoration: a session is held
    /// only when its whole assignment still matches a desired one, so a changed
    /// DIVISOR replaces every reviewer rather than leaving some partitioning by
    /// the old number.
    pub shard: nook_types::ShardAssignment,
}

/// One thing to do. Deliberately not "kill" — see [`Plan::stop`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Start a managed session in this checkout (the session's cwd is `path`).
    Start {
        checkout: NodeWorkspaceId,
        node: NodeId,
        path: String,
        /// Which slice of the declaration it is being started to own
        /// (MAIN-446). `SOLO` under [`Spread::PerCheckout`], where a checkout
        /// holds one session and there is nothing to divide.
        shard: nook_types::ShardAssignment,
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
    /// The workspace declared no ports, so it is held to one session per node
    /// (MAIN-361). Reported so a shortfall reads as "this is why" rather than as
    /// an unexplained failure to converge — the cap is deliberate, and it is a
    /// different condition from a pending clone or an ineligible fleet.
    pub capped: bool,
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
/// Whether the workspace has said what ports it binds (MAIN-361).
///
/// A parameter rather than a field on the spec, and not defaulted: every caller
/// has to answer it. An `Unsafe` workspace whose planner forgot to ask would
/// start a second session that binds the same hardcoded port as the first, and
/// the failure reads as the app's fault rather than nook's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortSafety {
    /// A declaration exists — including an explicit empty one, which is the repo
    /// saying it binds nothing.
    Declared,
    /// No declaration at all. Nook does not know what this repo binds, so it
    /// must assume two sessions would collide.
    Undeclared,
}

/// How a declaration turns `replicas` into placements.
///
/// The choice `Slots` asks a new managed purpose to make, in its other half:
/// that one says WHICH checkouts a declaration may use, this one says how many
/// sessions may share a node. They are named together at the one call site
/// where placement is declared, so a purpose cannot inherit either by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Spread {
    /// `replicas` counts NODES, and every checkout on a chosen node gets its
    /// own session. A workspace's own [`SessionSpec`] means this — "how many
    /// machines hold my repo, with a terminal in each checkout" — and it is
    /// what the reconcile-status endpoint reports against.
    PerCheckout,
    /// `replicas` counts SESSIONS, several of which may share one node — each
    /// node holding up to its own `max_loop_jobs` (MAIN-446).
    ///
    /// The review loop's rule, and the difference is what the number is FOR: a
    /// reviewer is not a copy of the repo, it is a share of the repo's open
    /// PRs. Three of them on one machine is a sensible thing to ask for, where
    /// three clones on one machine is not.
    Sharded,
}

pub fn plan(
    spec: &SessionSpec,
    nodes: &[NodeFacts],
    checkouts: &[CheckoutSlot],
    actual: &[Actual],
    ports: PortSafety,
    spread: Spread,
) -> Plan {
    let has_checkout = |id: NodeId| checkouts.iter().any(|c| c.node_id == id);
    // Eligible nodes, checkout-holders first so a bounded replica count prefers
    // an existing clone over cloning a fresh one; stable by id within a group.
    let mut eligible_nodes: Vec<&NodeFacts> = nodes.iter().filter(|n| eligible(spec, n)).collect();
    eligible_nodes.sort_by_key(|n| (!has_checkout(n.id), n.id.0));

    // THE CAP (MAIN-361 AC-3). A workspace that has not said what it binds gets
    // ONE session per node, whatever its spec asks for — because the second one
    // would bind whatever the app hardcoded, and so would the first.
    //
    // Per NODE, not per fleet: the collision is between processes on one
    // machine, so `Replicas::All` over three nodes still gets three sessions,
    // one each. Nothing here inspects the repo (NG-2); "no declaration" is the
    // whole detection.
    //
    // It binds a SHARDED declaration too, and deliberately: sharding divides
    // the PRs, not the ports, so two reviewers in one checkout are still two
    // agents that may each run whatever the repo's dev server is. A repo that
    // wants several reviewers on one box declares its ports — an explicit empty
    // declaration is enough — and the shortfall says so until it does.
    let capped = ports == PortSafety::Undeclared;

    match spread {
        Spread::PerCheckout => plan_per_checkout(spec, &eligible_nodes, checkouts, actual, capped),
        Spread::Sharded => plan_sharded(spec, &eligible_nodes, checkouts, actual, capped),
    }
}

/// `replicas` counts nodes; every checkout on a chosen node gets a session.
fn plan_per_checkout(
    spec: &SessionSpec,
    eligible_nodes: &[&NodeFacts],
    checkouts: &[CheckoutSlot],
    actual: &[Actual],
    capped: bool,
) -> Plan {
    let has_checkout = |id: NodeId| checkouts.iter().any(|c| c.node_id == id);
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

    // Applied AFTER the sort, so the surviving slot per node is deterministic —
    // two replicas planning the same instant must keep the same one.
    if capped {
        let mut seen: Vec<NodeId> = Vec::new();
        slots.retain(|c| {
            if seen.contains(&c.node_id) {
                false
            } else {
                seen.push(c.node_id);
                true
            }
        });
    }

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
                // One session per checkout, so there is nothing to divide.
                shard: nook_types::ShardAssignment::SOLO,
            });
        }
    }

    // Sessions land on real checkouts; the shortfall is the pending clones plus
    // replica slots that had no eligible node at all.
    out.placed = slots.len();
    out.desired = slots.len() + needs_clone.len() + unmet_nodes;
    out.shortfall = needs_clone.len() + unmet_nodes;
    out.capped = capped;
    out
}

/// `replicas` counts SESSIONS, and one node may hold several (MAIN-446).
///
/// Shards are dealt round-robin across the chosen nodes rather than filling one
/// before starting the next, so two reviewers on a two-node fleet land on two
/// machines. Within a node they cycle its checkouts, which for the review loop
/// is the single primary clone — several reviewers in one working copy, which
/// is fine because a reviewer reads PRs on the forge and never writes the tree.
///
/// The DIVISOR is the number actually PLACED, not the declared count. Dividing
/// by what was asked for hands the survivors slices belonging to reviewers
/// nobody created: an undeclared workspace at `max_replicas: 3` is held to one
/// session by MAIN-361's cap, and that one would take only PRs where
/// `number % 3 == 0` — two thirds of them unreviewed, permanently. NG-1's trade
/// covers a reviewer that is TEMPORARILY down and will come back to its shard;
/// a port cap is policy, and does not move the way capacity does.
///
/// This divides by capacity, not by liveness, which is what keeps AC-5 true: a
/// reviewer dying does not shrink the fleet's room for it, so the divisor holds
/// and the others keep their shards. Only a real capacity change — a node
/// leaving, a clone landing, ports being declared — repartitions.
fn plan_sharded(
    spec: &SessionSpec,
    eligible_nodes: &[&NodeFacts],
    checkouts: &[CheckoutSlot],
    actual: &[Actual],
    capped: bool,
) -> Plan {
    let capacity = |n: &NodeFacts| if capped { 1 } else { n.capacity() };
    let target = match spec.replicas {
        Replicas::Count { count } => count as usize,
        Replicas::Single => 1,
        // Every eligible node, filled. Nothing produces this today — the review
        // loop's spec is `Single` or a `Count` — but "all" has to mean the same
        // "as much as the fleet has" it means for nodes, not silently one each.
        Replicas::All => eligible_nodes.iter().map(|n| capacity(n)).sum(),
    };

    // The nodes worth involving: walk in order until their capacity covers the
    // target. Stopping early is what keeps `Count{1}` from cloning the repo onto
    // every loop node in the fleet just to leave it idle.
    let mut chosen: Vec<&NodeFacts> = Vec::new();
    let mut room = 0usize;
    for n in eligible_nodes {
        if room >= target {
            break;
        }
        room += capacity(n);
        chosen.push(n);
    }

    // A chosen node with NO checkout wants a clone — it can host nothing this
    // pass, and the next one places against it.
    let needs_clone: Vec<NodeId> = chosen
        .iter()
        .filter(|n| !checkouts.iter().any(|c| c.node_id == n.id))
        .map(|n| n.id)
        .collect();

    // Each chosen node with its checkouts, in the deterministic order two
    // replicas planning the same instant must agree on.
    let hosts: Vec<(&NodeFacts, Vec<&CheckoutSlot>)> = chosen
        .iter()
        .filter_map(|n| {
            let mut cos: Vec<&CheckoutSlot> =
                checkouts.iter().filter(|c| c.node_id == n.id).collect();
            cos.sort_by_key(|c| c.checkout_id.0);
            (!cos.is_empty()).then_some((*n, cos))
        })
        .collect();

    let mut slots: Vec<&CheckoutSlot> = Vec::new();
    let mut round = 0usize;
    while slots.len() < target {
        let before = slots.len();
        for (node, cos) in &hosts {
            if slots.len() >= target {
                break;
            }
            if round >= capacity(node) {
                continue;
            }
            slots.push(cos[round % cos.len()]);
        }
        // Nothing landed this round: every host is at its capacity, so another
        // round would spin forever on a target the fleet cannot reach.
        if slots.len() == before {
            break;
        }
        round += 1;
    }

    // WHERE they go is settled above; only now is WHAT they are handed knowable.
    let of = slots.len().max(1) as u32;
    let desired: Vec<(&CheckoutSlot, nook_types::ShardAssignment)> = slots
        .into_iter()
        .enumerate()
        .map(|(i, c)| {
            (
                c,
                nook_types::ShardAssignment {
                    index: i as u32,
                    of,
                },
            )
        })
        .collect();

    let mut out = Plan {
        needs_clone,
        ..Default::default()
    };

    // A session is held only when its WHOLE assignment is still wanted — same
    // checkout, same index, same divisor. Matching on the checkout alone would
    // keep a reviewer partitioning by a count nobody declares any more.
    let mut held: Vec<(NodeWorkspaceId, nook_types::ShardAssignment)> = Vec::new();
    for a in actual {
        let want = desired
            .iter()
            .any(|(c, s)| c.checkout_id == a.checkout_id && *s == a.shard);
        if want && !held.contains(&(a.checkout_id, a.shard)) {
            held.push((a.checkout_id, a.shard));
        } else {
            out.stop(a);
        }
    }

    for (c, shard) in &desired {
        if !held.contains(&(c.checkout_id, *shard)) {
            out.actions.push(Action::Start {
                checkout: c.checkout_id,
                node: c.node_id,
                path: c.path.clone(),
                shard: *shard,
            });
        }
    }

    out.placed = desired.len();
    out.desired = target;
    // Everything the declaration asked for that no node could host: a pending
    // clone, a fleet with no loop node at all, or a count past the capacity of
    // the ones there are (AC-6). Reported, never capped away.
    out.shortfall = target.saturating_sub(desired.len());
    out.capped = capped;
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

/// Paces clone-on-demand per (workspace, node).
///
/// A flat retry interval is not enough, and prod proved it: a repo the node's
/// key cannot read fails identically forever, so five nodes × four workspaces
/// re-issued a clone every minute for as long as the switch was on — a
/// permanent outbound storm against the git host, and enough traffic on the
/// node link to start dropping session messages. Two rules fix that class:
///
///   1. **Geometric backoff on consecutive failures**, from [`CLONE_RETRY_MIN`]
///      to [`CLONE_RETRY_MAX`], reset by a success. A transient failure recovers
///      in a minute; a permanent one settles to a check twice an hour.
///   2. **One attempt in flight per pair.** A clone may take up to 15 minutes;
///      with a 60s retry the old code could stack fifteen concurrent clones of
///      the same repo onto the same node, each fighting for the same directory.
///
/// In-process and per-replica: two replicas may each issue one initial clone,
/// which git handles (the second lands on an existing/in-progress directory and
/// fails harmlessly). It only ever grows by (workspace, node) pairs the fleet
/// actually has, so it needs no eviction.
///
/// Cloned handles share one map — `start_clone` keeps a handle so the spawned
/// task can report the outcome back, which is what makes the backoff respond to
/// reality rather than merely to the clock.
#[derive(Default, Clone)]
pub struct CloneThrottle {
    inner: std::sync::Arc<std::sync::Mutex<std::collections::HashMap<Pair, Attempt>>>,
}

type Pair = (WorkspaceId, NodeId);

#[derive(Default)]
struct Attempt {
    at: Option<std::time::Instant>,
    /// Consecutive failures; zeroed by a success.
    failures: u32,
    in_flight: bool,
}

const CLONE_RETRY_MIN: Duration = Duration::from_secs(60);
const CLONE_RETRY_MAX: Duration = Duration::from_secs(30 * 60);

impl CloneThrottle {
    /// How long to wait after `failures` consecutive failures. `1 << 5` already
    /// exceeds the cap, so the shift cannot overflow into a short interval.
    fn backoff(failures: u32) -> Duration {
        CLONE_RETRY_MIN
            .saturating_mul(1u32 << failures.min(5))
            .min(CLONE_RETRY_MAX)
    }

    /// True at most once per backoff window per pair, and never while an
    /// attempt for that pair is still outstanding. Marks the attempt in flight;
    /// the caller MUST report the outcome with [`CloneThrottle::record`].
    fn may_issue(&self, workspace: WorkspaceId, node: NodeId) -> bool {
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry((workspace, node)).or_default();
        if entry.in_flight {
            return false;
        }
        if let Some(at) = entry.at {
            if at.elapsed() < Self::backoff(entry.failures) {
                return false;
            }
        }
        entry.at = Some(std::time::Instant::now());
        entry.in_flight = true;
        true
    }

    /// Report an attempt's outcome. Returns how long the next attempt will wait,
    /// so the failure log can say when it will be retried instead of leaving a
    /// reader to guess from a stream of identical lines.
    fn record(&self, workspace: WorkspaceId, node: NodeId, ok: bool) -> Duration {
        let mut map = self.inner.lock().unwrap();
        let entry = map.entry((workspace, node)).or_default();
        entry.in_flight = false;
        entry.failures = if ok {
            0
        } else {
            entry.failures.saturating_add(1)
        };
        Self::backoff(entry.failures)
    }
}

async fn run(state: AppState) {
    let mut switch = SwitchLog::default();
    let clones = CloneThrottle::default();
    loop {
        // Re-read every tick, so a flip lands within one interval with no
        // restart. With every tenant off this is one indexed lookup and the
        // pass ends — "off" is quiet, not merely ineffective.
        if !switch.observe(any_enabled(&*state.settings).await) {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }
        if let Err(e) = pass(&state, &clones).await {
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
/// Public so a test can drive one pass and assert what it did NOT do.
///
/// Removing access reconciliation is a change nothing could otherwise see: the
/// planner tests exercise `plan_workspace` directly and stay green whether or
/// not anything calls it with `ManagedPurpose::Access`. That is the same shape
/// of gap that let the Stop notification bug ship — a guard proved at the layer
/// below the one the user experiences.
/// NOTE (MAIN-455): this pass no longer starts SESSIONS at all. Access
/// reconciliation went first (a terminal is not drift to be corrected), and the
/// review loop has now moved to headless runs — so what is left is one job:
/// converge review runs against the work the forge reports.
///
/// The session PLANNER below is still live, but only as the thing
/// `/reconcile-status` and `/review-loop-status` report through. Nothing calls
/// it to place anything any more.
pub async fn pass(state: &AppState, clones: &CloneThrottle) -> crate::error::ApiResult<()> {
    for (tenant, value) in state.settings.tenants_with_value(KEY).await? {
        // The stored value is arbitrary JSON; a row can exist reading `false`.
        // Same truthiness as the per-tenant gate, so on/off agree everywhere.
        if !truthy(Some(&value)) {
            continue;
        }
        // Once per tenant, not once per workspace: the review loop is agent
        // work, so it answers to the loops switch as well as to this one.
        let loops_on = crate::services::loops::enabled(&*state.settings, tenant).await;
        // Who a converged run is attributed to: the tenant's owner, the same
        // identity `nook operator` acts as. Resolved once per tenant rather than
        // once per workspace, and a tenant without one raises nothing — a job
        // whose `requested_by` dangles cannot mint an executor token, so an
        // invented id would fail later and less clearly.
        let owner = match state.identity.tenant_owner_user_id(tenant.0).await {
            Ok(Some(u)) => nook_types::UserId(u),
            Ok(None) => {
                tracing::warn!(%tenant, "no tenant owner to attribute review runs to — skipped");
                continue;
            }
            Err(e) => {
                tracing::warn!(%tenant, error = %e, "could not resolve the tenant owner");
                continue;
            }
        };
        for ws in state.workspaces.list(tenant).await? {
            // NO ACCESS RECONCILIATION. A terminal you opened is yours, and the
            // control plane does not have an opinion about how many of them
            // there should be.
            //
            // This used to run `ManagedPurpose::Access` over `Slots::EveryCheckout`
            // for every workspace in the tenant, so every checkout was owed a
            // session forever. Stopping a terminal dropped `actual` below
            // `desired` and the next pass started a replacement — Stop undid
            // itself within a poll interval. In production that read as
            // `desired=9 actual=12 stopping=3` with `cannot stop a managed
            // session — node offline; retrying next pass` every ten seconds,
            // indefinitely, because the reconciler also could not carry out the
            // stops it wanted.
            //
            // The declarative engine is still the right thing; it was pointed at
            // the wrong noun. A repo is a durable thing worth converging — a
            // tmux session is not a repo, and a human opening and closing
            // terminals is not drift to be corrected. What stays managed is the
            // REVIEW LOOP below: system work, per workspace, that nobody opens
            // by hand and that genuinely should come back on its own.
            if !loops_on {
                continue;
            }
            // A review is a RUN, not a session (MAIN-455). The declaration
            // still rules — `review_loop_max_replicas` is the ceiling on how
            // many run at once — but the unit is a pull request, and the
            // reconciler converges one headless run per PR that has moved.
            //
            // Nothing here starts a tmux session. A managed run is not
            // attachable on a machine, has a transcript, and cannot block on an
            // interactive prompt nobody is there to answer.
            // The repo still has to BE on the nodes that review it. This
            // converges the checkouts (MAIN-317's clone-on-demand) and no
            // longer starts anything — see `reconcile_workspace`.
            // The workspace's own forge identity, when it configured one
            // (MAIN-456); the fleet variable is only the single-tenant
            // fallback.
            let gh_token = crate::services::workspace_gh_token(state, tenant, ws.id).await;
            let open_prs = state
                .review_demand
                .open_prs(ws.id, ws.git_remote_url.as_deref(), gh_token.as_deref())
                .await;
            if let Err(e) = reconcile_workspace(
                state,
                tenant,
                ws.id,
                &review_loop_spec(ws.review_loop_max_replicas, open_prs),
                clones,
                ManagedPurpose::ReviewLoop,
                Slots::ClonesOnly,
                Spread::Sharded,
            )
            .await
            {
                tracing::warn!(workspace = %ws.id, error = %e, "workspace clone reconcile failed");
            }

            // A fleet that deployed BEFORE MAIN-455 still carries live tmux
            // review sessions, and deleting the reconciler's Stop arm left
            // nothing that would ever end them — their rows sit `running`
            // forever while headless runs do the actual reviewing beside them.
            // This is the retired scale-down run to ZERO: kill first, mark the
            // row only if the node took it, exactly the ordering the old arm
            // used, and a node that is offline keeps its row live so the next
            // pass tries again. On a fleet born after MAIN-455 the list is
            // empty and this costs one query.
            match state
                .sessions
                .live_managed(tenant, ws.id, Some(ManagedPurpose::ReviewLoop))
                .await
            {
                Ok(stale) => {
                    for sess in stale {
                        let (session_id, node_id) = (sess.id, sess.node_id);
                        if !state.registry.send_to_node(
                            node_id,
                            nook_proto::ControlToNode::KillSession { session_id },
                        ) {
                            tracing::warn!(
                                workspace = %ws.id, session = %session_id,
                                "cannot stop a legacy review session — node offline; retrying next pass"
                            );
                            continue;
                        }
                        if let Err(e) = state.sessions.mark_ended(tenant, session_id).await {
                            tracing::warn!(workspace = %ws.id, session = %session_id, error = %e, "legacy review session stop failed");
                        } else {
                            tracing::info!(
                                workspace = %ws.id, session = %session_id,
                                "stopped a legacy tmux review session — reviews are headless runs now (MAIN-455)"
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(workspace = %ws.id, error = %e, "could not list legacy review sessions")
                }
            }

            let ceiling = ws.review_loop_max_replicas.unwrap_or(1).max(0) as usize;
            let source = crate::services::work_source::ReviewWork {
                demand: &state.review_demand,
                token: gh_token,
            };
            match crate::services::run_reconcile::converge(
                state,
                &source,
                tenant,
                owner,
                ws.id,
                ws.git_remote_url.as_deref(),
                ceiling,
                None,
            )
            .await
            {
                Ok(c) if c.raised > 0 || c.withheld > 0 => tracing::info!(
                    workspace = %ws.id,
                    raised = c.raised,
                    live = c.live,
                    withheld = c.withheld,
                    ceiling,
                    "raised review runs"
                ),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(workspace = %ws.id, error = %e, "review run reconcile failed")
                }
            }

            // The build loop's POLL trigger (MAIN-458). Gated on the tenant's
            // `loops.enabled` alone — owner ruling 2026-08-08, option (a):
            // every workspace in a loops-on tenant converges its agent-ready
            // cards; MAIN-385's per-workspace switch refines this later.
            if loops_on {
                match crate::services::jobs::converge_builds(state, tenant, owner, ws.id, None)
                    .await
                {
                    Ok(c) if c.raised > 0 || c.withheld > 0 => tracing::info!(
                        workspace = %ws.id,
                        raised = c.raised,
                        live = c.live,
                        withheld = c.withheld,
                        "raised build runs"
                    ),
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(workspace = %ws.id, error = %e, "build run reconcile failed")
                    }
                }
            }
        }
    }
    Ok(())
}

/// The control plane's OWN declaration for a repo (MAIN-326): an always-on
/// review loop on the deployment's loop node, as many as the workspace asks for
/// (MAIN-445).
///
/// Still not the workspace's `SessionSpec` — that one says what a PERSON may
/// attach to, and these two must not be able to adopt or stop each other's
/// sessions. What MAIN-445 changed is the COUNT, and only the count: the
/// runtime and the `role=loop` selector remain this build's, not a caller's. A
/// repo still cannot ask for its reviewer on somebody's laptop.
///
/// `max_replicas` is the workspace's column, and it is a CEILING rather than a
/// count. `open_prs` is what the forge measured against it (MAIN-448), and the
/// whole computation is one line: **`desired = min(open_prs, ceiling)`**. A repo
/// with two open PRs gets two reviewers, a repo with twelve gets the ceiling,
/// and a quiet repo gets none.
///
/// The CEILING's three states are three different statements:
///
/// - `None` — unset. A ceiling of one, the value every workspace had before it
///   was settable.
/// - `Some(0)` — off. This repo is never reviewed, whatever the forge says.
/// - `Some(n)` — at most n reviewers, which MAIN-446 places for real: several
///   on one node if that is what the fleet has, each owning a shard of the
///   repo's open PRs, bounded by the node's own `max_loop_jobs`. A count past
///   what the fleet can host is still honest shortfall rather than a silent cap
///   — reporting the gap is the point.
///
/// A negative value cannot arrive — the route rejects it (MAIN-445 AC-2) — but
/// the cast saturates at zero rather than wrapping, because a column is a wider
/// contract than the one endpoint that writes it today.
///
/// **Zero from a quiet repo and zero from `Some(0)` are the same NUMBER and not
/// the same STATE, and the difference is the whole of AC-3.** Both stop the
/// session this repo has, through the same scale-down path. What separates them
/// is what happens next: a quiet repo's `open_prs` becomes 1 the moment somebody
/// opens a PR and the reviewer returns, where a zeroed repo's `min` is zero for
/// every count there is and it never comes back. Nothing here has to remember
/// which case it is in — the arithmetic already distinguishes them, which is why
/// it is arithmetic and not a flag.
///
/// **`open_prs` is `None` for "we do not know"**, and that is not zero either
/// (AC-6). No remote, a local path, a self-hosted forge, a deployment with no
/// token, an outage that has never once succeeded — all of them mean nothing
/// measured the demand, and the honest answer is to run what the repo declared.
/// No forge must never mean no reviewers.
/// WHERE a review may run: the fleet's loop nodes, and nowhere else.
///
/// One definition, read by the declaration that sizes reviewers and by the
/// dispatcher that places a run (MAIN-455 AC-4). A repo still cannot ask for
/// its reviewer on somebody's laptop, and moving reviews from sessions to runs
/// did not get to change that by omission.
pub(crate) fn review_loop_selector() -> std::collections::BTreeMap<String, String> {
    // The per-role KEY, not `role=loop` (MAIN-463): one `role` key made loop
    // and build mutually exclusive on a machine meant to do both. Old-style
    // `role=loop` nodes still match — `placement_of` widens them to this key.
    [("role/loop".to_string(), "true".to_string())]
        .into_iter()
        .collect()
}

pub(crate) fn review_loop_spec(max_replicas: Option<i32>, open_prs: Option<u32>) -> SessionSpec {
    let ceiling = match max_replicas {
        None => 1,
        Some(n) => n.max(0) as u32,
    };
    SessionSpec {
        runtime: crate::services::jobs::LOOP_RUNTIME.into(),
        node_selector: review_loop_selector(),
        tolerations: vec![],
        replicas: Replicas::Count {
            count: match open_prs {
                Some(prs) => prs.min(ceiling),
                None => ceiling,
            },
        },
    }
}

/// Which of a workspace's checkouts a declaration may place into.
///
/// One variant, deliberately: the review loop is the only managed purpose left,
/// and it takes only primary clones — it reviews the REPO's open PRs, which is
/// one job per repo per node however many worktrees sit beside the clone.
///
/// Kept as an enum rather than collapsed away because the DISTINCTION is the
/// thing worth keeping. `EveryCheckout` existed for per-worktree access
/// sessions; a future managed purpose that genuinely wants every worktree
/// should have to name that choice here rather than find placement hardcoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Slots {
    ClonesOnly,
}

/// Has this workspace said what ports it binds (MAIN-361 AC-1/AC-2)?
///
/// DERIVED ON READ, never stored. A stored flag would drift the moment a
/// declaration landed — the same reasoning `nook-types` already gives for
/// `is_blocked`, and the reason AC-8 needs nothing cleared: the cap lifts
/// because the next read computes a different answer, not because anybody
/// remembered to.
///
/// The check is on the STORED DECLARATION, not on whether a `.nook.toml`
/// exists. A workspace configured by hand through the API or the UI is safe
/// with no file in its repo, which is what keeps this independent of the file
/// card. And an explicit EMPTY declaration is SAFE: the repo said it binds
/// nothing, and that statement is exactly as good as naming a listener.
pub async fn port_safety(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> crate::error::ApiResult<PortSafety> {
    let declared = state
        .workspaces
        .get(tenant, workspace)
        .await?
        .and_then(|w| w.port_requirements)
        // A stored `null` is not a declaration — the column's absence and the
        // JSON null mean the same thing here.
        .filter(|v| !v.is_null())
        .is_some();
    Ok(if declared {
        PortSafety::Declared
    } else {
        PortSafety::Undeclared
    })
}

/// The checkouts a declaration may place into, narrowed by `allowed`.
///
/// `None` is every present checkout — per-worktree placement, what a person
/// wants. `Some(ids)` keeps only those, which is how the review loop is held to
/// primary clones: it reviews the REPO's PRs, so a node with a clone and three
/// worktrees still runs exactly one of them (MAIN-326).
fn placement_slots(
    present: Vec<crate::repo::workspaces::PresentCheckout>,
    allowed: Option<&[NodeWorkspaceId]>,
) -> Vec<CheckoutSlot> {
    present
        .into_iter()
        .filter(|c| allowed.is_none_or(|ids| ids.contains(&c.id)))
        .map(|c| CheckoutSlot {
            checkout_id: c.id,
            node_id: c.node_id,
            path: c.path,
        })
        .collect()
}

/// What a declaration would converge to right now: the plan, and the sessions
/// it was planned against.
///
/// Lifted out of [`reconcile_workspace`] so `GET /workspaces/{id}/
/// review-loop-status` reports the plan the loop ACTS on instead of a parallel
/// calculation (MAIN-447 AC-4). Two readers of one function cannot disagree;
/// two functions computing the same number always eventually do — which is why
/// `spread` is a parameter here rather than a constant chosen per caller: the
/// status must divide the same way the pass does, or it reports somebody else's
/// plan.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn plan_now(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    spec: &SessionSpec,
    purpose: ManagedPurpose,
    slots: Slots,
    spread: Spread,
) -> crate::error::ApiResult<(Plan, Vec<Actual>)> {
    let nodes = node_facts(state, tenant).await?;
    // Which checkouts this declaration may use. `clone_hosts` is the existing
    // "rows that make a node a placement host" read, so `ClonesOnly` reuses the
    // definition of a clone rather than restating it.
    let allowed: Option<Vec<NodeWorkspaceId>> = match slots {
        Slots::ClonesOnly => Some(
            state
                .workspaces
                .clone_hosts(tenant, workspace)
                .await?
                .into_iter()
                .map(|(_, id)| id)
                .collect(),
        ),
    };
    let checkouts = placement_slots(
        state
            .workspaces
            .present_checkouts(tenant, workspace)
            .await?,
        allowed.as_deref(),
    );
    let actual: Vec<Actual> = state
        .sessions
        .live_managed(tenant, workspace, Some(purpose))
        .await?
        .into_iter()
        .map(|s| Actual {
            session_id: s.id,
            checkout_id: s.checkout_id,
            node_id: s.node_id,
            shard: nook_types::ShardAssignment {
                index: s.managed_shard.max(0) as u32,
                of: s.managed_shards.max(1) as u32,
            },
        })
        .collect();

    // The workspace's own answer about what it binds (MAIN-361). Derived from
    // the stored declaration on every pass, never cached: a declaration landing
    // must lift the cap by itself on the next pass, with nothing to clear.
    let ports = port_safety(state, tenant, workspace).await?;
    let plan = plan(spec, &nodes, &checkouts, &actual, ports, spread);
    Ok((plan, actual))
}

// One declaration's convergence for one workspace: what it wants, where it may
// go, and how it divides. Grouping them behind a struct would name the same
// fields for the single call site that builds them.
#[allow(clippy::too_many_arguments)]
async fn reconcile_workspace(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    spec: &SessionSpec,
    clones: &CloneThrottle,
    purpose: ManagedPurpose,
    slots: Slots,
    spread: Spread,
) -> crate::error::ApiResult<()> {
    let (plan, actual) = plan_now(state, tenant, workspace, spec, purpose, slots, spread).await?;

    // AC-4's "logs desired-vs-actual". One line per workspace per pass, and
    // only when there is something to say — a converged workspace is silent, or
    // a fleet at rest would be the noisiest thing in the log.
    if !plan.actions.is_empty() || plan.shortfall > 0 || !plan.needs_clone.is_empty() {
        tracing::info!(
            %workspace,
            // Two declarations now converge per workspace, so an unlabelled
            // line would read as one loop contradicting itself.
            purpose = %purpose,
            desired = plan.desired,
            actual = actual.len(),
            starting = plan.actions.iter().filter(|a| matches!(a, Action::Start { .. })).count(),
            stopping = plan.actions.iter().filter(|a| matches!(a, Action::Stop { .. })).count(),
            shortfall = plan.shortfall,
            // Without this a port cap and a fleet that is simply too small read
            // identically — `desired=3 actual=1 shortfall=2` forever — and they
            // have completely different remedies.
            capped = plan.capped,
            needs_clone = plan.needs_clone.len(),
            "reconciling workspace"
        );
    }

    // NO SESSIONS ARE STARTED OR STOPPED HERE ANY MORE (MAIN-455).
    //
    // This function keeps the half of its job that is about the REPO: a node
    // that matches the declaration but has no checkout still gets one, so the
    // fleet's loop nodes hold the repos they are meant to review. What it no
    // longer does is put a tmux session on them — review work is a headless run
    // with a transcript now, raised per pull request by `run_reconcile`.
    //
    // The plan's `Start`/`Stop` actions are therefore computed and not acted
    // on. They still drive the log line above (and `/review-loop-status`), which
    // is what makes "this repo wants three reviewers and the fleet can host one"
    // answerable; acting on them is what put an attachable terminal on a machine.

    // Clone-on-demand (MAIN-317): an eligible node that matched the spec but has
    // no checkout is no longer just a reported shortfall — we clone the workspace
    // onto it so the NEXT pass can place a session there. Throttled, because a
    // repo that cannot be cloned would otherwise be re-issued every pass.
    for node in &plan.needs_clone {
        if clones.may_issue(workspace, *node) {
            start_clone(state, tenant, workspace, *node, clones.clone()).await;
        }
    }
    Ok(())
}

/// Ask a node to clone the workspace's repo. Fire-and-forget: discovery lands
/// the `node_workspaces` row and the next pass places the session, so holding
/// the pass on a multi-minute clone would only stall every other workspace.
///
/// Carries the workspace's pinned credential when it has one (MAIN-367), and
/// the node's own reach when it does not — a local path, or a public remote.
async fn start_clone(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    node: NodeId,
    clones: CloneThrottle,
) {
    let url = match state.workspaces.git_remote_url(workspace, tenant).await {
        Ok(Some(Some(url))) => url,
        // No remote to clone from, or the workspace vanished — nothing to do.
        _ => {
            clones.record(workspace, node, false);
            tracing::warn!(%workspace, node = %node, "clone-on-demand: no git remote to clone");
            return;
        }
    };
    // Whose tree this checkout belongs in. The REQUESTING tenant, which on a
    // cross-tenant placement is not the node's own (MAIN-363).
    let tenant_slug = crate::services::tenant_slug(state, tenant).await;

    // The workspace's own key (MAIN-367). This is what the "no credential"
    // comment here used to admit was missing: without it every clone went out
    // with the node's generated key, which no private repo authorizes, so a
    // loop could never start on one. `None` still means "the node's own reach",
    // which is what public repos and local paths need.
    let ssh_key = crate::services::workspace_git_key(state, tenant, workspace).await;
    let Some(rx) =
        state
            .registry
            .request_op(node, |request_id| nook_proto::ControlToNode::CloneRepo {
                request_id,
                url: url.clone(),
                dest_name: None,
                ssh_key: ssh_key.clone(),
                tenant_slug: tenant_slug.clone(),
            })
    else {
        // Went offline between planning and issuing; the next pass re-reads.
        // Not a failure of the repo — do not let a flapping node back off a
        // clone that has never actually been tried.
        clones.record(workspace, node, true);
        return;
    };
    tracing::info!(%workspace, node = %node, url = %url, "clone-on-demand: cloning workspace onto node");
    // Log the outcome so a repeatedly-failing clone is visible rather than a
    // silent perpetual shortfall — but do NOT block the pass on it.
    let state = state.clone();
    tokio::spawn(async move {
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(900), rx).await;
        // Record BEFORE the reporting below: the backoff must move even if
        // recording the checkout then fails, or a repo that clones fine but
        // cannot be associated would retry at the floor interval forever.
        let ok = matches!(&outcome, Ok(Ok(p)) if p.ok);
        let retry_in = clones.record(workspace, node, ok);
        match outcome {
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
                                tracing::info!(%workspace, node = %node, %path, "clone-on-demand: cloned and recorded checkout");
                                // A new checkout wants the workspace's `.env`,
                                // and the control plane cannot deliver it — the
                                // secrets are sealed with a password the server
                                // never sees. So it ANNOUNCES, and an unlocked
                                // browser replays the unlock, which is what
                                // actually writes the file (MAIN-317 AC-2, as
                                // re-scoped by the 2026-08-04 ruling).
                                //
                                // Discovery has always done this. Pinning the
                                // row here with `associate_clone`, instead of
                                // waiting for the node's next scan, is what
                                // made the reconciler skip it — so a checkout
                                // the RECONCILER created never announced and no
                                // browser ever learned to push env to it.
                                //
                                // Not gated on the row being new, unlike
                                // discovery's call: `associate_clone` is
                                // idempotent and does not report which it did.
                                // It costs nothing to be wrong here — clone-on-
                                // demand only runs for a node with no checkout,
                                // `announce_new_checkout` returns immediately
                                // when the workspace has no secrets, and a
                                // repeat announce re-pushes the same file.
                                crate::services::secrets::announce_new_checkout(
                                    &state, tenant, workspace, node, &path,
                                )
                                .await;
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
            // Every failure line carries the next retry, so a reader can tell a
            // backing-off loop from a hammering one without timing the log.
            Ok(Ok(p)) => {
                tracing::warn!(%workspace, node = %node, message = %p.message, retry_in_s = retry_in.as_secs(), "clone-on-demand: clone failed")
            }
            Ok(Err(_)) => {
                tracing::warn!(%workspace, node = %node, retry_in_s = retry_in.as_secs(), "clone-on-demand: node disconnected mid-clone")
            }
            Err(_) => {
                tracing::warn!(%workspace, node = %node, retry_in_s = retry_in.as_secs(), "clone-on-demand: clone timed out")
            }
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
    // The tenant's own nodes PLUS any node owned by one of its members, wherever
    // homed. MAIN-353 made a person's machines reachable from every org they
    // belong to on the SEE and USE paths and deliberately left placement behind
    // (NG-3); this closes that half, because "my own nodes" that a second org's
    // work never reaches is not what owning them was for.
    for node in state.nodes.placement_candidates(tenant).await? {
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
        // The node's own loop-job ceiling (MAIN-446), read from the same
        // `capabilities` blob the dispatcher reads it from — absent means an
        // agent too old to report it, not a node that runs nothing.
        let max_loop_jobs = node
            .capabilities
            .get("max_loop_jobs")
            .and_then(|v| v.as_u64())
            .map(|n| n as u32);
        out.push(NodeFacts {
            id: node.id,
            online: state.registry.node_online(node.id),
            labels: placement.labels,
            taints: placement.taints,
            runtimes,
            max_loop_jobs,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (WorkspaceId, NodeId) {
        (
            WorkspaceId(uuid::Uuid::now_v7()),
            NodeId(uuid::Uuid::now_v7()),
        )
    }

    /// A repo that can never be cloned must not be retried at a fixed interval
    /// forever — that is the prod storm this backoff exists to prevent.
    #[test]
    fn clone_backoff_grows_with_consecutive_failures_and_caps() {
        assert_eq!(CloneThrottle::backoff(0), CLONE_RETRY_MIN);
        assert_eq!(CloneThrottle::backoff(1), CLONE_RETRY_MIN * 2);
        assert_eq!(CloneThrottle::backoff(4), CLONE_RETRY_MIN * 16);
        // Past the cap it stops growing rather than wrapping to something short.
        assert_eq!(CloneThrottle::backoff(5), CLONE_RETRY_MAX);
        assert_eq!(CloneThrottle::backoff(u32::MAX), CLONE_RETRY_MAX);
    }

    #[test]
    fn a_success_returns_the_pair_to_the_floor_interval() {
        let (ws, node) = ids();
        let t = CloneThrottle::default();
        assert!(t.may_issue(ws, node));
        for _ in 0..3 {
            t.record(ws, node, false);
        }
        assert_eq!(t.record(ws, node, true), CLONE_RETRY_MIN);
    }

    /// The other half of the storm: a clone may take up to 15 minutes, so a
    /// throttle that only looked at elapsed time would stack a fresh attempt on
    /// top of the outstanding one every minute.
    #[test]
    fn only_one_attempt_per_pair_is_in_flight() {
        let (ws, node) = ids();
        let t = CloneThrottle::default();
        assert!(t.may_issue(ws, node));
        assert!(
            !t.may_issue(ws, node),
            "second attempt while one is running"
        );
        t.record(ws, node, false);
        // Still inside the backoff window, so it stays refused — but for the
        // window now, not because something is outstanding.
        assert!(!t.may_issue(ws, node));
    }

    /// Handles share state: `start_clone` reports the outcome through a clone of
    /// the throttle, and that must reach the map the pass reads.
    #[test]
    fn cloned_handles_share_one_map() {
        let (ws, node) = ids();
        let t = CloneThrottle::default();
        let handle = t.clone();
        assert!(t.may_issue(ws, node));
        handle.record(ws, node, false);
        assert!(!t.may_issue(ws, node), "backoff must apply across handles");
    }

    /// The planner as it behaves for a workspace that HAS declared its ports —
    /// which every case below is about except the cap's own tests. Keeps those
    /// cases reading as they did before MAIN-361 added the parameter.
    fn plan_safe(
        spec: &SessionSpec,
        nodes: &[NodeFacts],
        checkouts: &[CheckoutSlot],
        actual: &[Actual],
    ) -> Plan {
        plan(
            spec,
            nodes,
            checkouts,
            actual,
            PortSafety::Declared,
            Spread::PerCheckout,
        )
    }
    use nook_types::{NodeTaint, ShardAssignment, Toleration};
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
            // Unreported, so the sharded cases below run against
            // `CAPACITY_WHEN_UNREPORTED` unless they say otherwise — the same
            // number a node in the field gets for not saying.
            max_loop_jobs: None,
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
        sharded(session, cid, on, ShardAssignment::SOLO)
    }

    /// A running session that owns one shard of a divided declaration.
    fn sharded(session: u8, cid: u8, on: u8, shard: ShardAssignment) -> Actual {
        Actual {
            session_id: SessionId(Uuid::from_u128(1000 + session as u128)),
            checkout_id: NodeWorkspaceId(Uuid::from_u128(2000 + cid as u128)),
            node_id: NodeId(Uuid::from_u128(on as u128)),
            shard,
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
        let p = plan_safe(
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
        let p = plan_safe(&spec(Replicas::Single), &nodes, &checkouts, &[]);
        assert_eq!(start_checkouts(&p).len(), 2, "one session per checkout");
        assert_eq!(starts(&p), vec![nodes[0].id, nodes[0].id], "both on node 1");
        assert_eq!(p.desired, 2);
        assert_eq!(p.shortfall, 0);
    }

    #[test]
    fn an_empty_selector_matches_every_node() {
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let p = plan_safe(&spec(Replicas::All), &nodes, &[co(1), co(2)], &[]);
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
            starts(&plan_safe(&s, &nodes, &[co(1), co(2), co(3)], &[])),
            vec![nodes[1].id]
        );
    }

    #[test]
    fn an_untolerated_taint_refuses_however_well_the_labels_match() {
        let s = spec(Replicas::All);
        let nodes = [node(1, &[], &[("no-loops", "NoSchedule")])];
        let p = plan_safe(&s, &nodes, &[co(1)], &[]);
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
        assert_eq!(starts(&plan_safe(&s, &tolerated, &[co(1)], &[])).len(), 1);
        assert!(starts(&plan_safe(&s, &other_effect, &[co(2)], &[])).is_empty());
    }

    #[test]
    fn an_offline_node_is_not_a_placement() {
        let mut n = node(1, &[], &[]);
        n.online = false;
        let p = plan_safe(&spec(Replicas::Single), &[n], &[co(1)], &[]);
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
        let p = plan_safe(
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
        let p = plan_safe(&spec(Replicas::Count { count: 2 }), &nodes, &[co(1)], &[]);
        assert_eq!(starts(&p).len(), 1);
        assert_eq!(p.needs_clone, vec![NodeId(Uuid::from_u128(2))]);
        assert_eq!(p.shortfall, 1);
    }

    #[test]
    fn all_counts_nodes_awaiting_a_clone_as_shortfall_not_as_a_lower_target() {
        // `All` means every eligible node holds the repo. If a chosen node has no
        // checkout, the target does not quietly shrink to hide it.
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let p = plan_safe(&spec(Replicas::All), &nodes, &[co(1)], &[]);
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
        let p = plan_safe(
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
        let p = plan_safe(
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
        let p = plan_safe(
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
        let p = plan_safe(&s, &nodes, &[co(1)], &[running(1, 1, 1)]);
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
            starts(&plan_safe(&spec(Replicas::Single), &a, &cos, &[])),
            starts(&plan_safe(&spec(Replicas::Single), &b, &cos, &[]))
        );
    }

    #[test]
    fn zero_replicas_stops_everything_and_starts_nothing() {
        // MAIN-315 made "managed, wanting none" expressible on purpose. It has
        // to mean something here, and what it means is: stop them.
        let nodes = [node(1, &[], &[])];
        let p = plan_safe(
            &spec(Replicas::Count { count: 0 }),
            &nodes,
            &[co(1)],
            &[running(1, 1, 1)],
        );
        assert!(starts(&p).is_empty());
        assert_eq!(stops(&p).len(), 1);
    }

    // ── the port-safety cap (MAIN-361) ──────────────────────────────────────

    /// AC-3, and the case the card is named for: `All` over three nodes gives
    /// one session per NODE, not one in total. The collision is between
    /// processes on one machine, so the fleet still spreads.
    #[test]
    fn an_undeclared_workspace_gets_one_session_per_node_not_one_in_total() {
        let nodes = [node(1, &[], &[]), node(2, &[], &[]), node(3, &[], &[])];
        let cos = [co(1), co(2), co(3)];
        let p = plan(
            &spec(Replicas::All),
            &nodes,
            &cos,
            &[],
            PortSafety::Undeclared,
            Spread::PerCheckout,
        );
        assert_eq!(starts(&p).len(), 3, "one per node, all three nodes");
        assert!(p.capped);
    }

    /// The cap is per node, so several worktrees on ONE machine collapse to one
    /// — which is exactly the collision it exists to prevent.
    #[test]
    fn several_worktrees_on_one_node_collapse_to_a_single_session() {
        let nodes = [node(1, &[], &[])];
        // Three checkouts, all on node 1.
        let cos = [
            CheckoutSlot {
                checkout_id: NodeWorkspaceId(Uuid::from_u128(10)),
                node_id: nodes[0].id,
                path: "/a".into(),
            },
            CheckoutSlot {
                checkout_id: NodeWorkspaceId(Uuid::from_u128(11)),
                node_id: nodes[0].id,
                path: "/b".into(),
            },
            CheckoutSlot {
                checkout_id: NodeWorkspaceId(Uuid::from_u128(12)),
                node_id: nodes[0].id,
                path: "/c".into(),
            },
        ];
        let declared = plan(
            &spec(Replicas::All),
            &nodes,
            &cos,
            &[],
            PortSafety::Declared,
            Spread::PerCheckout,
        );
        assert_eq!(starts(&declared).len(), 3, "declared: one per worktree");

        let capped = plan(
            &spec(Replicas::All),
            &nodes,
            &cos,
            &[],
            PortSafety::Undeclared,
            Spread::PerCheckout,
        );
        assert_eq!(starts(&capped).len(), 1, "undeclared: one for the machine");
        assert!(capped.capped);
    }

    /// A big `Count` is capped just as `All` is — the spec does not buy its way
    /// out (AC-3).
    #[test]
    fn a_count_of_five_is_capped_the_same_way() {
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let cos = [co(1), co(2)];
        let p = plan(
            &spec(Replicas::Count { count: 5 }),
            &nodes,
            &cos,
            &[],
            PortSafety::Undeclared,
            Spread::PerCheckout,
        );
        assert_eq!(starts(&p).len(), 2, "two nodes, one session each");
        // The ask is still reported honestly: desired reflects the spec, so the
        // UI can say 5 wanted / 2 placed and name the cap as the reason.
        assert_eq!(p.desired, 5);
        assert_eq!(p.shortfall, 3);
    }

    /// The cap chooses the SAME slot on every replica, or two of them would
    /// start different sessions for the same machine.
    #[test]
    fn which_worktree_survives_the_cap_is_deterministic() {
        let nodes = [node(1, &[], &[])];
        let a = CheckoutSlot {
            checkout_id: NodeWorkspaceId(Uuid::from_u128(20)),
            node_id: nodes[0].id,
            path: "/a".into(),
        };
        let b = CheckoutSlot {
            checkout_id: NodeWorkspaceId(Uuid::from_u128(21)),
            node_id: nodes[0].id,
            path: "/b".into(),
        };
        let one = plan(
            &spec(Replicas::All),
            &nodes,
            &[a.clone(), b.clone()],
            &[],
            PortSafety::Undeclared,
            Spread::PerCheckout,
        );
        let other = plan(
            &spec(Replicas::All),
            &nodes,
            &[b, a],
            &[],
            PortSafety::Undeclared,
            Spread::PerCheckout,
        );
        assert_eq!(one.actions, other.actions);
    }

    /// AC-8: the cap lifts because the next plan computes a different answer.
    /// Nothing is closed, cleared or remembered — the same inputs with a
    /// declaration simply produce the uncapped plan.
    #[test]
    fn declaring_lifts_the_cap_with_nothing_else_changing() {
        let nodes = [node(1, &[], &[])];
        let cos = [
            CheckoutSlot {
                checkout_id: NodeWorkspaceId(Uuid::from_u128(30)),
                node_id: nodes[0].id,
                path: "/a".into(),
            },
            CheckoutSlot {
                checkout_id: NodeWorkspaceId(Uuid::from_u128(31)),
                node_id: nodes[0].id,
                path: "/b".into(),
            },
        ];
        let before = plan(
            &spec(Replicas::All),
            &nodes,
            &cos,
            &[],
            PortSafety::Undeclared,
            Spread::PerCheckout,
        );
        let after = plan(
            &spec(Replicas::All),
            &nodes,
            &cos,
            &[],
            PortSafety::Declared,
            Spread::PerCheckout,
        );
        assert_eq!(starts(&before).len(), 1);
        assert_eq!(starts(&after).len(), 2);
        assert!(before.capped && !after.capped);
    }

    #[test]
    fn absent_is_off_and_that_is_the_whole_point() {
        assert!(!truthy(None));
        assert!(!truthy(Some(&serde_json::json!(false))));
        assert!(truthy(Some(&serde_json::json!(true))));
        assert!(truthy(Some(&serde_json::json!("true"))));
    }

    // ── MAIN-326: the control plane's own declaration ────────────────────────

    /// A node that reports the `claude` runtime, so the review-loop spec's
    /// runtime check does not decide these cases for us.
    fn loop_capable(n: u8, labels: &[(&str, &str)]) -> NodeFacts {
        // Fixtures describe what `placement_of` PRODUCES — so they go THROUGH
        // the production widening rather than mirroring it. A mirrored copy is
        // the prod reviewer's exact #350 finding: delete the production call
        // and this suite stays green while every old-style node stops matching.
        let mut facts = NodeFacts {
            runtimes: vec!["claude".into()],
            ..node(n, labels, &[])
        };
        crate::routes::nodes::widen_role_labels(&mut facts.labels);
        facts
    }

    /// MAIN-463 AC-1's core promise, new-style only: a node labeled with
    /// per-role KEYS — `role/loop=true` beside `role/build=true`, no `role=<v>`
    /// anywhere — takes review work. Nothing here touches the widening shim,
    /// which is the point: the keys are the native form and the old syntax is
    /// the compatibility case.
    #[test]
    fn a_node_wearing_both_role_keys_takes_review_work() {
        let both = loop_capable(1, &[("role/loop", "true"), ("role/build", "true")]);
        let p = plan(
            &review_loop_spec(None, None),
            &[both],
            &[checkout(1, 1)],
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(
            placements(&p).len(),
            1,
            "per-role keys are the native form — no role=<v> label required"
        );
    }

    /// AC-2. The whole targeting rule is one label, so this is the test that
    /// says a person's laptop never gets handed the fleet's review loop.
    #[test]
    fn the_review_loop_only_lands_on_a_loop_node() {
        let user_box = loop_capable(1, &[]);
        let loop_box = loop_capable(2, &[("role", "loop")]);
        let nodes = [user_box.clone(), loop_box.clone()];
        let cos = [checkout(1, 1), checkout(2, 2)];

        let p = plan(
            &review_loop_spec(None, None),
            &nodes,
            &cos,
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(starts(&p), vec![loop_box.id], "only the loop node");
        assert!(!starts(&p).contains(&user_box.id));
    }

    /// A fleet with no loop node at all reports a shortfall rather than placing
    /// the loop somewhere convenient — the failure has to be visible, because a
    /// repo silently going unreviewed looks exactly like a repo with no PRs.
    #[test]
    fn no_loop_node_is_a_shortfall_not_a_substitute() {
        let nodes = [loop_capable(1, &[]), loop_capable(2, &[])];
        let p = plan(
            &review_loop_spec(None, None),
            &nodes,
            &[checkout(1, 1)],
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert!(starts(&p).is_empty());
        assert_eq!(p.shortfall, 1);
    }

    /// `Single`, and the reason for it: a second loop node must not mean two
    /// agents posting the same verdict on the same PR.
    #[test]
    fn two_loop_nodes_still_get_one_review_loop() {
        let nodes = [
            loop_capable(1, &[("role", "loop")]),
            loop_capable(2, &[("role", "loop")]),
        ];
        let p = plan(
            &review_loop_spec(None, None),
            &nodes,
            &[checkout(1, 1), checkout(2, 2)],
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(starts(&p).len(), 1);
    }

    fn present(id: u8, node: u8) -> crate::repo::workspaces::PresentCheckout {
        crate::repo::workspaces::PresentCheckout {
            id: NodeWorkspaceId(Uuid::from_u128(2000 + id as u128)),
            node_id: NodeId(Uuid::from_u128(node as u128)),
            path: format!("/w/{id}"),
        }
    }

    /// AC-1's "per repo". The loop node holds the clone and two worktrees; the
    /// access declaration wants a terminal in each, the review loop wants one
    /// reviewer for the repo — not three agents racing each other's PRs.
    #[test]
    fn the_review_loop_takes_the_clone_and_not_the_worktrees() {
        let clone = present(1, 1);
        let all = vec![clone.clone(), present(2, 1), present(3, 1)];

        assert_eq!(placement_slots(all.clone(), None).len(), 3, "access: each");
        let only_clone = placement_slots(all, Some(&[clone.id]));
        assert_eq!(only_clone.len(), 1);
        assert_eq!(only_clone[0].checkout_id, clone.id);
    }

    /// The spec is the control plane's, not a workspace's, and its values are
    /// what the rest of this card is built on — a selector typo would silently
    /// place nothing at all, which reads as "no loop node" forever.
    #[test]
    fn the_review_loop_spec_is_what_the_card_declares() {
        let s = review_loop_spec(None, None);
        assert_eq!(s.runtime, "claude");
        assert_eq!(
            s.node_selector.get("role/loop").map(String::as_str),
            Some("true"),
            "the per-role key (MAIN-463) — one `role` value made loop and build exclusive"
        );
        // A `Count`, not `Replicas::Single`, since MAIN-448: the spec is now
        // always the RESULT of `min(open_prs, ceiling)`, and `Single` cannot
        // express the zero a quiet repo needs. Unset with no forge is one, which
        // is the number `Single` meant.
        assert_eq!(s.replicas, Replicas::Count { count: 1 });
    }

    /// MAIN-445 AC-5, the upgrade guarantee, stated where it can actually fail:
    /// an unset workspace must produce the SAME PLAN as the hardcoded spec did,
    /// not merely a non-empty one. The literal below is the pre-change
    /// `Replicas::Single` — if a future edit makes "unset" mean anything else,
    /// every existing deployment silently re-places its reviewers and this is
    /// the test that says so.
    #[test]
    fn unset_reconciles_exactly_as_the_hardcoded_spec_did() {
        let pre_change = SessionSpec {
            runtime: crate::services::jobs::LOOP_RUNTIME.into(),
            node_selector: [("role".to_string(), "loop".to_string())]
                .into_iter()
                .collect(),
            tolerations: vec![],
            replicas: Replicas::Single,
        };
        let nodes = [
            loop_capable(1, &[("role", "loop")]),
            loop_capable(2, &[("role", "loop")]),
            loop_capable(3, &[]),
        ];
        let cos = [checkout(1, 1), checkout(2, 2), checkout(3, 3)];

        let old = plan(
            &pre_change,
            &nodes,
            &cos,
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        let new = plan(
            &review_loop_spec(None, None),
            &nodes,
            &cos,
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(starts(&new), starts(&old));
        assert_eq!(new.desired, old.desired);
        assert_eq!(new.shortfall, old.shortfall);
    }

    /// AC-3's "0 means off, not unmanaged". The distinction is invisible in the
    /// desired count — both are zero — and shows up only as the STOP: an
    /// unmanaged workspace would leave the session running forever.
    #[test]
    fn zero_stops_the_review_loop_it_already_has() {
        let loop_box = loop_capable(1, &[("role", "loop")]);
        let cos = [checkout(1, 1)];
        let running = [Actual {
            session_id: SessionId(Uuid::from_u128(900)),
            checkout_id: cos[0].checkout_id,
            node_id: loop_box.id,
            shard: ShardAssignment::SOLO,
        }];

        let p = plan(
            &review_loop_spec(Some(0), None),
            &[loop_box],
            &cos,
            &running,
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(p.desired, 0);
        assert!(starts(&p).is_empty(), "0 starts nothing");
        assert_eq!(stops(&p), vec![running[0].session_id], "and stops the one");
    }

    /// AC-3 again, from the other side: unset must NOT stop the session that 0
    /// stops. Written as its own test because a bug collapsing None into
    /// Some(0) would turn every workspace in the fleet off at once, and the
    /// test above would still pass.
    #[test]
    fn unset_keeps_the_review_loop_zero_would_stop() {
        let loop_box = loop_capable(1, &[("role", "loop")]);
        let cos = [checkout(1, 1)];
        let running = [Actual {
            session_id: SessionId(Uuid::from_u128(900)),
            checkout_id: cos[0].checkout_id,
            node_id: loop_box.id,
            shard: ShardAssignment::SOLO,
        }];

        let p = plan(
            &review_loop_spec(None, None),
            &[loop_box],
            &cos,
            &running,
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(p.desired, 1);
        assert!(stops(&p).is_empty(), "unset stops nothing");
    }

    // ── MAIN-446: several reviewers on one node, sharded by PR ───────────────

    /// A loop node that reports a capacity, so the sharded cases below are
    /// bounded by a number the test names rather than by the unreported default.
    fn loop_node(n: u8, cap: u32) -> NodeFacts {
        NodeFacts {
            max_loop_jobs: Some(cap),
            ..loop_capable(n, &[("role", "loop")])
        }
    }

    /// Every start, as (node, shard index, divisor) — the three things a
    /// placement IS once a declaration can be divided.
    fn placements(p: &Plan) -> Vec<(NodeId, u32, u32)> {
        p.actions
            .iter()
            .filter_map(|a| match a {
                Action::Start { node, shard, .. } => Some((*node, shard.index, shard.of)),
                _ => None,
            })
            .collect()
    }

    /// AC-1, and the whole point of the card: the one loop node this fleet has
    /// runs all three reviewers, instead of one and a shortfall of two.
    #[test]
    fn three_reviewers_fit_on_one_loop_node_with_the_capacity_for_them() {
        let p = plan(
            &review_loop_spec(Some(3), None),
            &[loop_node(1, 3)],
            &[checkout(1, 1)],
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        let node = NodeId(Uuid::from_u128(1));
        assert_eq!(
            placements(&p),
            vec![(node, 0, 3), (node, 1, 3), (node, 2, 3)],
            "three sessions, one machine, three distinct shards"
        );
        assert_eq!(p.shortfall, 0);
        assert_eq!(p.desired, 3);
    }

    /// AC-1's other half: the cap is the NODE's existing `max_loop_jobs`, not a
    /// number this declaration invents. A node quiesced to zero hosts no
    /// reviewer at all, exactly as it takes no loop job.
    #[test]
    fn the_ceiling_is_the_nodes_own_max_loop_jobs() {
        let quiesced = plan(
            &review_loop_spec(Some(3), None),
            &[loop_node(1, 0)],
            &[checkout(1, 1)],
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert!(placements(&quiesced).is_empty(), "0 means 0");
        assert_eq!(quiesced.shortfall, 3);

        // Unreported reads as the shipped default, which is the same number a
        // node that never set `NOOK_MAX_LOOP_JOBS` runs loop jobs at.
        let unreported = plan(
            &review_loop_spec(Some(9), None),
            &[loop_capable(1, &[("role", "loop")])],
            &[checkout(1, 1)],
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(
            placements(&unreported).len(),
            crate::services::jobs::CAPACITY_WHEN_UNREPORTED as usize
        );
    }

    /// AC-6. Asking for more than the fleet can host is reported, never capped
    /// away — and the reviewers that DID land divide the repo between just
    /// themselves, so every PR still has an owner.
    ///
    /// Dividing by the declared count instead would hand these two `of=3` and
    /// leave a third of the repo owned by a reviewer nobody placed. The count
    /// is still reported as short, which is the honest part; the coverage is
    /// not something to be honest about losing.
    ///
    /// This divides by CAPACITY, not by liveness — NG-1's trade is untouched.
    /// A reviewer whose session died has not shrunk the room the fleet has for
    /// it, so the divisor holds and its PRs wait for it to come back.
    #[test]
    fn a_count_past_capacity_divides_between_the_reviewers_that_landed() {
        let p = plan(
            &review_loop_spec(Some(3), None),
            &[loop_node(1, 2)],
            &[checkout(1, 1)],
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        let node = NodeId(Uuid::from_u128(1));
        assert_eq!(placements(&p), vec![(node, 0, 2), (node, 1, 2)]);
        assert_eq!(p.placed, 2);
        assert_eq!(
            p.desired, 3,
            "what was asked for is still what was asked for"
        );
        assert_eq!(p.shortfall, 1, "the third is reported, not hidden");

        // The two of them between them own everything — the property the
        // declared divisor broke.
        let shards: Vec<ShardAssignment> = placements(&p)
            .into_iter()
            .map(|(_, index, of)| ShardAssignment { index, of })
            .collect();
        assert!(
            (1u64..200).all(|pr| shards.iter().filter(|s| s.owns(pr)).count() == 1),
            "every PR owned exactly once by the reviewers that exist"
        );
    }

    /// Two loop nodes get one reviewer each before either gets two. Not a
    /// balancing requirement (NG-2) — it is that a reviewer is agent work, and
    /// stacking the second onto the first machine while the second sits idle is
    /// the surprising reading of "run two".
    #[test]
    fn reviewers_spread_across_nodes_before_they_stack() {
        let p = plan(
            &review_loop_spec(Some(3), None),
            &[loop_node(1, 2), loop_node(2, 2)],
            &[checkout(1, 1), checkout(2, 2)],
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        let (a, b) = (NodeId(Uuid::from_u128(1)), NodeId(Uuid::from_u128(2)));
        assert_eq!(placements(&p), vec![(a, 0, 3), (b, 1, 3), (a, 2, 3)]);
    }

    /// A declaration the fleet already satisfies produces NO actions — the
    /// baseline AC-5 is measured against, and the thing a per-pass reshuffle
    /// would break first.
    #[test]
    fn a_satisfied_sharded_declaration_is_a_no_op() {
        let spec = review_loop_spec(Some(3), None);
        let nodes = [loop_node(1, 3)];
        let cos = [checkout(1, 1)];
        let live: Vec<Actual> = (0..3)
            .map(|i| sharded(i as u8, 1, 1, ShardAssignment { index: i, of: 3 }))
            .collect();

        let p = plan(
            &spec,
            &nodes,
            &cos,
            &live,
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert!(p.actions.is_empty(), "converged: {:?}", p.actions);
    }

    /// AC-5. One reviewer dying must not re-partition the two still running:
    /// the pass starts its shard back, and touches nothing else. The failure
    /// this pins is a placement derived from POSITION in the live list, which
    /// renumbers every survivor the moment one is missing.
    #[test]
    fn restarting_one_reviewer_leaves_the_others_shards_alone() {
        let spec = review_loop_spec(Some(3), None);
        let nodes = [loop_node(1, 3)];
        let cos = [checkout(1, 1)];
        // Shard 1 is gone; 0 and 2 are still up.
        let survivors = [
            sharded(0, 1, 1, ShardAssignment { index: 0, of: 3 }),
            sharded(2, 1, 1, ShardAssignment { index: 2, of: 3 }),
        ];

        let p = plan(
            &spec,
            &nodes,
            &cos,
            &survivors,
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(
            placements(&p),
            vec![(NodeId(Uuid::from_u128(1)), 1, 3)],
            "only the missing shard comes back"
        );
        assert!(stops(&p).is_empty(), "and nothing running is disturbed");
    }

    /// The other side of AC-5: a changed DIVISOR is a real repartition, so
    /// every reviewer is replaced rather than some carrying on dividing the
    /// repo by a number nobody declares any more.
    #[test]
    fn changing_the_count_replaces_every_reviewer() {
        let nodes = [loop_node(1, 3)];
        let cos = [checkout(1, 1)];
        let live = [
            sharded(0, 1, 1, ShardAssignment { index: 0, of: 3 }),
            sharded(1, 1, 1, ShardAssignment { index: 1, of: 3 }),
            sharded(2, 1, 1, ShardAssignment { index: 2, of: 3 }),
        ];

        let p = plan(
            &review_loop_spec(Some(2), None),
            &nodes,
            &cos,
            &live,
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(stops(&p).len(), 3, "all three divided by the old number");
        assert_eq!(
            placements(&p),
            vec![
                (NodeId(Uuid::from_u128(1)), 0, 2),
                (NodeId(Uuid::from_u128(1)), 1, 2)
            ]
        );
    }

    /// AC-4, as a property over a range rather than one hand-picked example:
    /// for every division, each PR belongs to exactly one shard — the union is
    /// everything and the intersection is empty. This is the only thing keeping
    /// two reviewers off the same PR, since nothing claims and nothing locks.
    #[test]
    fn the_shard_filter_partitions_every_pr_exactly_once() {
        for of in 1..=5u32 {
            for pr in 1..200u64 {
                let owners: Vec<u32> = (0..of)
                    .filter(|index| ShardAssignment { index: *index, of }.owns(pr))
                    .collect();
                assert_eq!(
                    owners.len(),
                    1,
                    "PR #{pr} split {of} ways is owned by {owners:?}"
                );
            }
        }
    }

    /// An unsharded reviewer owns everything, which is what makes the change a
    /// no-op for every deployment that never sets a count — and what an absent
    /// `NOOK_REVIEW_SHARDS` has to keep meaning in the skill.
    #[test]
    fn one_shard_owns_every_pr() {
        assert!((1..50).all(|pr| ShardAssignment::SOLO.owns(pr)));
    }

    /// The MAIN-361 cap still binds a sharded declaration, and this is the test
    /// that says so out loud: a repo that has not declared its ports gets ONE
    /// reviewer per node however many it asks for. Sharding divides the PRs, not
    /// the ports — two agents in one checkout can still each start whatever the
    /// repo's dev server is. Declaring (even an empty list) lifts it.
    #[test]
    fn an_undeclared_workspace_still_gets_one_reviewer_per_node() {
        let nodes = [loop_node(1, 3)];
        let cos = [checkout(1, 1)];
        let capped = plan(
            &review_loop_spec(Some(3), None),
            &nodes,
            &cos,
            &[],
            PortSafety::Undeclared,
            Spread::Sharded,
        );
        // The `of` is the point, not just the count. Handing the survivor
        // `of=3` would leave it reviewing only PRs divisible by three, with the
        // other two shards belonging to reviewers the cap will never allow —
        // permanently, because a port cap is policy and does not move.
        assert_eq!(placements(&capped), vec![(nodes[0].id, 0, 1)]);
        assert!(capped.capped, "and the plan says why");
        assert_eq!(
            capped.shortfall, 2,
            "still honest about what it cannot host"
        );

        let declared = plan(
            &review_loop_spec(Some(3), None),
            &nodes,
            &cos,
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(placements(&declared).len(), 3);
    }

    /// The skill and the control plane must describe the SAME contract. The
    /// skill is prose an agent follows, so nothing but its text can be checked
    /// — and its text is compiled into the node binary, which is what makes
    /// this a test rather than a hope.
    ///
    /// The contract changed with MAIN-455: a run is TOLD its pull request
    /// (`NOOK_REVIEW_PR`), and the shard arithmetic this used to assert is
    /// retired — so the test now guards against the modulo INSTRUCTIONS
    /// reappearing as much as for the directive being taught.
    #[test]
    fn the_skill_states_the_directive_this_module_raises_runs_for() {
        let skill = include_str!("../../../../skills/nook-review/SKILL.md");
        assert!(
            skill.contains("NOOK_REVIEW_PR"),
            "the reviewer skill must read the directive the run env carries"
        );
        assert!(
            !skill.contains("number % NOOK_REVIEW_SHARDS"),
            "the modulo partition is retired; teaching it again would have a directed reviewer filtering a queue it no longer owns"
        );
        assert!(
            skill.contains("nook reviews verdict"),
            "the skill must deliver its conclusion through the verdict call — the control plane posts, the agent only concludes"
        );
        assert!(
            !skill.contains("gh pr comment"),
            "posting by gh is retired; an agent-formatted comment is the drift the verdict call exists to end"
        );
    }
    // ── MAIN-448: the count comes from the forge ─────────────────────────────

    /// How many reviewers a ceiling and a forge count add up to. The whole of
    /// AC-2 is this one function, so the table is the test.
    fn desired(cap: Option<i32>, open_prs: Option<u32>) -> u32 {
        match review_loop_spec(cap, open_prs).replicas {
            Replicas::Count { count } => count,
            other => panic!("the review loop always declares a Count, got {other:?}"),
        }
    }

    /// AC-2, as the ticket's own examples.
    #[test]
    fn desired_is_the_smaller_of_the_open_prs_and_the_ceiling() {
        assert_eq!(desired(Some(3), Some(12)), 3, "12 PRs, cap 3");
        assert_eq!(desired(Some(3), Some(2)), 2, "2 PRs, cap 3");
        assert_eq!(desired(Some(3), Some(3)), 3, "exactly at the cap");
        assert_eq!(desired(None, Some(9)), 1, "unset is a ceiling of one");
    }

    /// AC-6. Four different reasons nothing measured the demand, one answer:
    /// run what the repo declared. **No forge must never mean no reviewers**, so
    /// this is the case that has to keep working when everything else does not.
    #[test]
    fn no_forge_runs_the_declared_ceiling_exactly_as_before() {
        assert_eq!(desired(None, None), 1);
        assert_eq!(desired(Some(3), None), 3);
        assert_eq!(desired(Some(0), None), 0, "an off repo is still off");
    }

    /// AC-3, first direction: a repo with nothing open wants no reviewer.
    #[test]
    fn a_quiet_repo_desires_no_reviewer() {
        assert_eq!(desired(None, Some(0)), 0);
        assert_eq!(
            desired(Some(5), Some(0)),
            0,
            "the ceiling cannot conjure work"
        );
    }

    /// AC-3, the distinction the card calls the likeliest way to ship a fleet
    /// that silently stops reviewing. Both states desire ZERO and are not the
    /// same state — what separates them is what a PR does next.
    #[test]
    fn a_quiet_repo_comes_back_and_a_zeroed_one_never_does() {
        // Quiet → busy: the count moves, so the desire moves with it.
        assert_eq!(desired(None, Some(0)), 0);
        assert_eq!(
            desired(None, Some(1)),
            1,
            "a PR appeared: the reviewer returns"
        );

        // Off → still off, for every count there is. This is the assertion that
        // fails if somebody "fixes" the zero case by treating 0 as unset.
        for prs in [0, 1, 5, 50] {
            assert_eq!(
                desired(Some(0), Some(prs)),
                0,
                "an explicitly zeroed repo must not come back for {prs} PRs"
            );
        }
    }

    /// AC-3's stop half, at the layer that actually stops things: desiring zero
    /// has to reach the running session through the ordinary scale-down path,
    /// not through a special case.
    #[test]
    fn scaling_to_zero_stops_the_reviewer_and_a_pr_starts_it_again() {
        let loop_box = loop_capable(1, &[("role", "loop")]);
        let cos = [checkout(1, 1)];
        let running = [Actual {
            session_id: SessionId(Uuid::from_u128(900)),
            checkout_id: cos[0].checkout_id,
            node_id: loop_box.id,
            shard: ShardAssignment::SOLO,
        }];

        let quiet = plan(
            &review_loop_spec(None, Some(0)),
            std::slice::from_ref(&loop_box),
            &cos,
            &running,
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(quiet.desired, 0);
        assert_eq!(
            stops(&quiet),
            vec![running[0].session_id],
            "no PRs: it stops"
        );

        // …and with the session now gone, one PR brings it back.
        let busy = plan(
            &review_loop_spec(None, Some(1)),
            &[loop_box],
            &cos,
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(busy.desired, 1);
        assert_eq!(starts(&busy).len(), 1, "a PR appeared: it comes back");
    }

    /// AC-5. The forge changes the COUNT and nothing else — a repo still cannot
    /// get its reviewer onto somebody's laptop, and a node still hosts no more
    /// than its own `max_loop_jobs`.
    #[test]
    fn the_forge_changes_the_count_and_not_the_placement() {
        let busy = review_loop_spec(Some(9), Some(9));
        assert_eq!(busy.runtime, "claude");
        assert_eq!(
            busy.node_selector.get("role/loop").map(String::as_str),
            Some("true"),
            "still the loop-role key, whatever the forge says"
        );

        // Nine wanted, a node that runs two: two placed, seven reported.
        let p = plan(
            &busy,
            &[loop_node(1, 2)],
            &[checkout(1, 1)],
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert_eq!(
            placements(&p).len(),
            2,
            "bounded by max_loop_jobs (MAIN-446)"
        );
        assert_eq!(p.shortfall, 7);
    }

    /// A user's laptop is not a loop node, so a busy repo does not spill onto
    /// one. Stated separately because "the count went up" is exactly when a
    /// placement rule would be tempting to relax.
    #[test]
    fn a_busy_repo_still_does_not_reach_a_non_loop_node() {
        let user_box = loop_capable(1, &[]);
        let loop_box = loop_capable(2, &[("role", "loop")]);
        let p = plan(
            &review_loop_spec(Some(5), Some(5)),
            &[user_box.clone(), loop_box],
            &[checkout(1, 1), checkout(2, 2)],
            &[],
            PortSafety::Declared,
            Spread::Sharded,
        );
        assert!(
            !starts(&p).contains(&user_box.id),
            "twelve PRs do not buy a reviewer on somebody's laptop"
        );
    }
}
