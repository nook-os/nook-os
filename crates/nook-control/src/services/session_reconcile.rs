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
//! `(workspace_id, node_id)` over LIVE managed sessions means the loser's
//! insert fails and it moves on. No lease to expire, no lock to leak, no window
//! where both succeed. The same index is what makes "no doubling per node"
//! true rather than merely intended.
//!
//! ## What is deliberately not here
//!
//! A node that matches but has no checkout is reported as `needs_clone` and
//! skipped — cloning is MAIN-317 (NG-1). No UI or status surface — MAIN-319
//! (NG-2).

use std::collections::BTreeMap;
use std::time::Duration;

use nook_types::{NodeId, Replicas, SessionId, SessionSpec, TenantId, WorkspaceId};

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
    /// Whether the workspace's checkout already exists here. Cloning is
    /// MAIN-317, so a matching node without one is reported, not used.
    pub has_checkout: bool,
}

/// A live managed session, as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actual {
    pub session_id: SessionId,
    pub node_id: NodeId,
}

/// One thing to do. Deliberately not "kill" — see [`Plan::stop`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Start a managed session for this workspace on this node.
    Start { node: NodeId },
    /// Stop a managed session: either surplus after `replicas` dropped, or one
    /// whose node stopped being eligible. Carries the node because stopping
    /// means killing the process there, not only ending the row.
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
pub fn plan(spec: &SessionSpec, nodes: &[NodeFacts], actual: &[Actual]) -> Plan {
    let mut sorted: Vec<&NodeFacts> = nodes.iter().collect();
    sorted.sort_by_key(|n| n.id.0);

    let matching: Vec<&NodeFacts> = sorted.into_iter().filter(|n| eligible(spec, n)).collect();
    let (placeable, needs_clone): (Vec<&NodeFacts>, Vec<&NodeFacts>) =
        matching.iter().partition(|n| n.has_checkout);

    let desired = match spec.replicas {
        Replicas::Count { count } => count as usize,
        Replicas::Single => 1,
        // "One on every node that matches" counts the nodes it can actually
        // reach; a node awaiting a clone is a shortfall, not a lower target.
        Replicas::All => matching.len(),
    };

    let mut out = Plan {
        desired,
        needs_clone: needs_clone.iter().map(|n| n.id).collect(),
        ..Default::default()
    };

    // Keep what is already right. A managed session on a node that is no longer
    // eligible is stopped — the declaration moved, so the session must.
    let placeable_ids: Vec<NodeId> = placeable.iter().map(|n| n.id).collect();
    let mut keep: Vec<&Actual> = Vec::new();
    for a in actual {
        if placeable_ids.contains(&a.node_id) {
            keep.push(a);
        } else {
            out.stop(a);
        }
    }
    // Deterministic surplus: drop from the end of node order, so every replica
    // chooses the same victims.
    keep.sort_by_key(|a| a.node_id.0);

    if keep.len() > desired {
        for a in &keep[desired..] {
            out.stop(a);
        }
        keep.truncate(desired);
    }

    let held: Vec<NodeId> = keep.iter().map(|a| a.node_id).collect();
    for n in &placeable {
        if out
            .actions
            .iter()
            .filter(|a| matches!(a, Action::Start { .. }))
            .count()
            + keep.len()
            >= desired
        {
            break;
        }
        if !held.contains(&n.id) {
            out.actions.push(Action::Start { node: n.id });
        }
    }

    out.placed = keep.len()
        + out
            .actions
            .iter()
            .filter(|a| matches!(a, Action::Start { .. }))
            .count();
    out.shortfall = desired.saturating_sub(out.placed);
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

async fn run(state: AppState) {
    let mut switch = SwitchLog::default();
    loop {
        // Re-read every tick, so a flip lands within one interval with no
        // restart. With every tenant off this is one indexed lookup and the
        // pass ends — "off" is quiet, not merely ineffective.
        if !switch.observe(any_enabled(&*state.settings).await) {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }
        if let Err(e) = pass(&state).await {
            // A failed pass is not fatal: the next one re-reads the world from
            // scratch, which is the point of a reconciler over a queue.
            tracing::warn!(error = %e, "session reconcile pass failed");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// One pass over every workspace carrying a spec.
async fn pass(state: &AppState) -> crate::error::ApiResult<()> {
    for (tenant, workspace, spec) in managed_workspaces(state).await? {
        if !enabled(&*state.settings, tenant).await {
            continue;
        }
        if let Err(e) = reconcile_workspace(state, tenant, workspace, &spec).await {
            // Per workspace, so one broken declaration does not stop the fleet.
            tracing::warn!(%workspace, error = %e, "workspace reconcile failed");
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
) -> crate::error::ApiResult<()> {
    let nodes = node_facts(state, tenant, workspace).await?;
    let actual = state.sessions.live_managed(tenant, workspace).await?;
    let actual: Vec<Actual> = actual
        .into_iter()
        .map(|(session_id, node_id)| Actual {
            session_id,
            node_id,
        })
        .collect();

    let plan = plan(spec, &nodes, &actual);

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
            Action::Start { node } => {
                // Losing the race is the NORMAL outcome on a multi-replica
                // deployment — the unique index means the other replica already
                // started it. Debug, not warn: it is the mechanism working.
                if let Err(e) = start_managed(state, tenant, workspace, *node, spec).await {
                    tracing::debug!(%workspace, node = %node, error = %e, "managed start did not win");
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
    Ok(())
}

/// Facts for every node in the tenant, including whether this workspace's
/// checkout is already there.
async fn node_facts(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
) -> crate::error::ApiResult<Vec<NodeFacts>> {
    let mut out = Vec::new();
    // `None` owner: the reconciler places across the whole tenant fleet, not one
    // person's machines.
    for node in state.nodes.list(tenant, None).await? {
        let placement = crate::routes::nodes::placement_of(&node);
        let has_checkout = state
            .workspaces
            .clone_path(tenant, workspace, node.id)
            .await?
            .is_some();
        out.push(NodeFacts {
            id: node.id,
            online: state.registry.node_online(node.id),
            labels: placement.labels,
            taints: placement.taints,
            has_checkout,
        });
    }
    Ok(out)
}

async fn start_managed(
    state: &AppState,
    tenant: TenantId,
    workspace: WorkspaceId,
    node: NodeId,
    spec: &SessionSpec,
) -> crate::error::ApiResult<()> {
    let Some(path) = state.workspaces.clone_path(tenant, workspace, node).await? else {
        // Raced with a checkout being pruned; the next pass re-reads and
        // reports it as needing a clone.
        return Ok(());
    };
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
        &path,
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
            has_checkout: true,
        }
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
                Action::Start { node } => Some(*node),
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

    fn running(session: u8, on: u8) -> Actual {
        Actual {
            session_id: SessionId(Uuid::from_u128(1000 + session as u128)),
            node_id: NodeId(Uuid::from_u128(on as u128)),
        }
    }

    #[test]
    fn an_empty_selector_matches_every_node() {
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let p = plan(&spec(Replicas::All), &nodes, &[]);
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
        assert_eq!(starts(&plan(&s, &nodes, &[])), vec![nodes[1].id]);
    }

    #[test]
    fn an_untolerated_taint_refuses_however_well_the_labels_match() {
        let s = spec(Replicas::All);
        let nodes = [node(1, &[], &[("no-loops", "NoSchedule")])];
        let p = plan(&s, &nodes, &[]);
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
        assert_eq!(starts(&plan(&s, &tolerated, &[])).len(), 1);
        assert!(starts(&plan(&s, &other_effect, &[])).is_empty());
    }

    #[test]
    fn an_offline_node_is_not_a_placement() {
        let mut n = node(1, &[], &[]);
        n.online = false;
        let p = plan(&spec(Replicas::Single), &[n], &[]);
        assert!(starts(&p).is_empty());
        assert_eq!(p.shortfall, 1, "the desired session is still owed");
    }

    #[test]
    fn replicas_spread_one_per_node_and_never_double_up() {
        // AC-4: `replicas > eligible` places what it can and reports the rest.
        // Stacking two on one machine would satisfy the number and miss the
        // point of a spread.
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let p = plan(&spec(Replicas::Count { count: 5 }), &nodes, &[]);
        assert_eq!(starts(&p).len(), 2);
        assert_eq!(p.desired, 5);
        assert_eq!(p.placed, 2);
        assert_eq!(p.shortfall, 3);
    }

    #[test]
    fn a_node_without_the_checkout_needs_a_clone_rather_than_a_session() {
        // NG-1: cloning is MAIN-317. Reported, skipped, and still counted as a
        // shortfall — the workspace is not getting what it asked for.
        let mut without = node(2, &[], &[]);
        without.has_checkout = false;
        let nodes = [node(1, &[], &[]), without];
        let p = plan(&spec(Replicas::Count { count: 2 }), &nodes, &[]);
        assert_eq!(starts(&p).len(), 1);
        assert_eq!(p.needs_clone, vec![NodeId(Uuid::from_u128(2))]);
        assert_eq!(p.shortfall, 1);
    }

    #[test]
    fn all_counts_nodes_awaiting_a_clone_as_shortfall_not_as_a_lower_target() {
        // `All` means one per matching node. If a matching node has no
        // checkout, the target does not quietly shrink to hide it.
        let mut without = node(2, &[], &[]);
        without.has_checkout = false;
        let nodes = [node(1, &[], &[]), without];
        let p = plan(&spec(Replicas::All), &nodes, &[]);
        assert_eq!(p.desired, 2);
        assert_eq!(p.placed, 1);
        assert_eq!(p.shortfall, 1);
    }

    #[test]
    fn a_converged_workspace_does_nothing() {
        // Idempotence, which is the whole contract of a reconciler: running it
        // twice must not do anything the first run did not.
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let actual = [running(1, 1), running(2, 2)];
        let p = plan(&spec(Replicas::Count { count: 2 }), &nodes, &actual);
        assert!(p.actions.is_empty(), "{:?}", p.actions);
        assert_eq!(p.shortfall, 0);
    }

    #[test]
    fn a_crashed_session_is_replaced() {
        // The reconciler is handed only LIVE managed sessions, so a crash is
        // simply an actual that is no longer there — and the gap is filled.
        let nodes = [node(1, &[], &[]), node(2, &[], &[])];
        let p = plan(
            &spec(Replicas::Count { count: 2 }),
            &nodes,
            &[running(1, 1)],
        );
        assert_eq!(starts(&p), vec![NodeId(Uuid::from_u128(2))]);
    }

    #[test]
    fn dropping_replicas_stops_the_surplus() {
        let nodes = [node(1, &[], &[]), node(2, &[], &[]), node(3, &[], &[])];
        let actual = [running(1, 1), running(2, 2), running(3, 3)];
        let p = plan(&spec(Replicas::Single), &nodes, &actual);
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
        let p = plan(&s, &nodes, &[running(1, 1)]);
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
        assert_eq!(
            starts(&plan(&spec(Replicas::Single), &a, &[])),
            starts(&plan(&spec(Replicas::Single), &b, &[]))
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
            &[running(1, 1)],
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
