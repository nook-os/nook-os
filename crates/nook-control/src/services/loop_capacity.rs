//! How many loop jobs a node runs at once, and who decided (MAIN-508).
//!
//! Capacity used to live only in the node's process environment, so changing it
//! meant editing a unit file as root and `systemctl restart nook-node` — the one
//! operation that strands every in-flight streaming build. The port range had
//! already solved this shape (`port_leases::range_of`): an env var the node
//! advertises, PLUS a central value an operator sets from the API, the CLI or
//! the Nodes page. This is that, for the other half of the same sizing decision.
//!
//! ## Precedence, highest first (AC-3)
//!
//! 1. **The host's pin** — `NOOK_MAX_LOOP_JOBS_PINNED` truthy on the machine.
//!    The node's own number wins and the central write is REFUSED.
//! 2. **The operator's central value** — `nodes.max_loop_jobs`, including `0`.
//! 3. **What the node advertises** — `NOOK_MAX_LOOP_JOBS`, else the node's
//!    `DEFAULT_MAX_LOOP_JOBS`.
//! 4. **[`jobs::CAPACITY_WHEN_UNREPORTED`]** — an agent too old to report one.
//!
//! **Central beats the env, and that ordering is the feature.** The machine this
//! was built for is precisely the one whose unit file already names a number:
//! if the env won, retuning it would still cost a restart and nothing would have
//! changed. The pin exists so a host that genuinely must decide locally — sized
//! by something outside NookOS — keeps the last word, and it refuses the central
//! write rather than ignoring it, because a setting that silently does nothing
//! is worse than one that says no.
//!
//! `0` keeps meaning STOP CLAIMING at every level (AC-5): a deliberate cordon,
//! never "busy". That is why the central value is `Option<i32>` and not a
//! sentinel — absent and zero are different statements.

use nook_types::{Node, NodeCapacity};

use crate::services::jobs::CAPACITY_WHEN_UNREPORTED;

/// The most an operator may set centrally.
///
/// Not a machine limit — it is a typo limit. Nothing downstream breaks at a
/// large number, but a node handed four thousand agent jobs is somebody's
/// slipped keystroke, and the refusal costs a re-type where the alternative
/// costs a fleet.
pub const MAX_SETTABLE: i64 = 64;

/// What the node itself reports, if anything.
fn advertised(node: &Node) -> Option<u32> {
    node.capabilities
        .get("max_loop_jobs")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32)
}

/// Has this host pinned its own number? Absent reads as false — an older agent
/// does not report the flag, and the safe reading of silence is "not pinned",
/// which leaves the operator able to set it.
pub fn pinned(node: &Node) -> bool {
    node.capabilities
        .get("max_loop_jobs_pinned")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// The capacity in force for a node, and where it came from.
///
/// Pure, over a row the caller already holds: placement reads it per attempt,
/// so the value an operator sets is honoured on the next poll with no restart
/// and no reconnect (AC-2).
pub fn of(node: &Node) -> NodeCapacity {
    let advertised = advertised(node);
    let operator = node.max_loop_jobs.map(|n| n.max(0) as u32);
    let pinned = pinned(node);

    let (effective, source) = if pinned {
        (advertised.unwrap_or(CAPACITY_WHEN_UNREPORTED), "host")
    } else if let Some(n) = operator {
        (n, "operator")
    } else if let Some(n) = advertised {
        (n, "node")
    } else {
        (CAPACITY_WHEN_UNREPORTED, "default")
    };

    NodeCapacity {
        effective,
        source: source.into(),
        operator,
        advertised,
        pinned,
    }
}

/// Stamp the computed capacity onto rows on their way out of an endpoint, so a
/// CLI table or a UI badge reads it instead of re-deriving the precedence.
pub fn fill(nodes: &mut [Node]) {
    for n in nodes.iter_mut() {
        n.loop_capacity = Some(of(n));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(caps: serde_json::Value, operator: Option<i32>) -> Node {
        let now = chrono::Utc::now();
        Node {
            id: nook_types::NodeId::new(),
            tenant_id: nook_types::TenantId(uuid::Uuid::nil()),
            name: "azul".into(),
            hostname: String::new(),
            platform: String::new(),
            capabilities: caps,
            resources: serde_json::json!({}),
            status: "online".into(),
            last_seen_at: None,
            owner_person_id: None,
            shared: false,
            labels: serde_json::json!({}),
            taints: serde_json::json!([]),
            operator_authorize_optout: false,
            home_tenant: None,
            port_range_start: None,
            port_range_end: None,
            port_exclusions: None,
            max_loop_jobs: operator,
            loop_capacity: None,
            cordon: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// AC-6: a node reporting its own number and nothing set centrally behaves
    /// exactly as it did before this shipped.
    #[test]
    fn the_nodes_own_number_stands_until_an_operator_sets_one() {
        let c = of(&node(serde_json::json!({ "max_loop_jobs": 2 }), None));
        assert_eq!((c.effective, c.source.as_str()), (2, "node"));
    }

    /// The point of the card: the central value beats the machine's env, which
    /// is what makes retuning it possible without a restart.
    #[test]
    fn the_central_value_beats_what_the_node_advertises() {
        let c = of(&node(serde_json::json!({ "max_loop_jobs": 2 }), Some(6)));
        assert_eq!((c.effective, c.source.as_str()), (6, "operator"));
        assert_eq!((c.operator, c.advertised), (Some(6), Some(2)));
    }

    /// AC-5: zero is a cordon at either level, and is never read as "unset".
    #[test]
    fn zero_is_a_cordon_and_not_an_absent_value() {
        let c = of(&node(serde_json::json!({ "max_loop_jobs": 4 }), Some(0)));
        assert_eq!((c.effective, c.source.as_str()), (0, "operator"));
        let c = of(&node(serde_json::json!({ "max_loop_jobs": 0 }), None));
        assert_eq!((c.effective, c.source.as_str()), (0, "node"));
    }

    /// AC-3's escape hatch: the host keeps the last word when it asks for it,
    /// even against a central value that is already stored.
    #[test]
    fn a_pinned_host_wins_over_the_central_value() {
        let caps = serde_json::json!({ "max_loop_jobs": 3, "max_loop_jobs_pinned": true });
        let c = of(&node(caps, Some(9)));
        assert_eq!((c.effective, c.source.as_str()), (3, "host"));
        assert!(c.pinned);
        // Still reported, so the UI can say what is being overruled rather than
        // pretending nobody ever set it.
        assert_eq!(c.operator, Some(9));
    }

    /// An agent too old to report anything is unspecified, not zero — the
    /// reason `CAPACITY_WHEN_UNREPORTED` exists.
    #[test]
    fn an_unreporting_node_falls_back_rather_than_stopping() {
        let c = of(&node(serde_json::json!({}), None));
        assert_eq!(
            (c.effective, c.source.as_str()),
            (CAPACITY_WHEN_UNREPORTED, "default")
        );
    }
}
