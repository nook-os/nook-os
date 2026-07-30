//! Authorize a runtime once, deliver it to N machines, without a session
//! (MAIN-290, C3).
//!
//! C1 gave the control plane a device-flow driver; C2 gave it a message that
//! installs an opaque credential on a node. This is the orchestration between
//! them, and it is what replaces "spawn `claude auth login` in a terminal and
//! have someone drive it there".
//!
//! ## Why the endpoint returns before the flow finishes
//!
//! [`DeviceFlow::wait`] blocks until a human types a code into the provider's
//! site — seconds at best, minutes in practice, `expires_in` at worst. An HTTP
//! request cannot hold that open, so the endpoint starts the flow, hands back a
//! `flow_id`, and everything after runs in a spawned task that reports through
//! [`UiEvent`]s keyed by that id. The UI follows the flow on the socket it
//! already has.
//!
//! ## Why deliveries are correlated in memory
//!
//! A node reports `RuntimeCredentialInstalled { runtime, path, error }` — C2's
//! message, which carries no flow id. Rather than widen the wire format for a
//! correlation the node has no use for, the flow records which
//! `(node, runtime)` deliveries it is waiting on, and the socket handler asks.
//! An entry is consumed on the first report, so a later unrelated install
//! cannot be mistaken for this flow's.
//!
//! The map is in memory on purpose: a flow is alive only as long as the process
//! that started it. If the control plane restarts mid-flow the credential is
//! gone too — it was never persisted (C1's guarantee) — and the right answer is
//! to start again, not to resume something whose secret half no longer exists.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use nook_proto::{ControlToNode, UiEvent};
use nook_types::{NodeId, TenantId};
use uuid::Uuid;

use crate::services::runtime_auth::{DeviceFlow, RuntimeAuthError, RuntimeCredential};
use crate::state::AppState;

/// Deliveries a flow is still waiting to hear about, keyed by the pair the
/// node's report carries.
#[derive(Default)]
pub struct PendingDeliveries {
    inner: DashMap<(NodeId, String), Uuid>,
}

impl PendingDeliveries {
    pub fn new() -> Self {
        Self::default()
    }

    fn expect(&self, node: NodeId, runtime: &str, flow: Uuid) {
        self.inner.insert((node, runtime.to_string()), flow);
    }

    /// The flow a report belongs to, if any — consumed, so one report resolves
    /// one delivery and a later unrelated install is not attributed to a flow
    /// that has already finished.
    pub fn take(&self, node: NodeId, runtime: &str) -> Option<Uuid> {
        self.inner.remove(&(node, runtime.to_string())).map(|e| e.1)
    }

    /// Give up on a delivery nobody reported. Called when the node was never
    /// reachable, so the entry does not sit there capturing an unrelated
    /// install later.
    fn forget(&self, node: NodeId, runtime: &str) {
        self.inner.remove(&(node, runtime.to_string()));
    }
}

/// The error classes the UI branches on. Mirrors [`RuntimeAuthError`] rather
/// than flattening it: "the code expired" wants a *start again* button and
/// "you declined" does not.
fn error_kind(e: &RuntimeAuthError) -> &'static str {
    match e {
        RuntimeAuthError::Expired => "expired",
        RuntimeAuthError::Denied => "denied",
        RuntimeAuthError::Provider(_) => "provider",
        RuntimeAuthError::Transport(_) => "transport",
    }
}

/// Run one authorization to completion, reporting through `UiEvent`s.
///
/// Spawned by the endpoint; never awaited by a request. Every exit path emits
/// something, because a flow that goes quiet is indistinguishable from one that
/// is still waiting.
pub fn spawn(
    state: AppState,
    tenant: TenantId,
    flow_id: Uuid,
    flow: DeviceFlow,
    nodes: Vec<NodeId>,
) {
    tokio::spawn(async move {
        let runtime = flow.runtime().to_string();

        let pending = match flow.begin().await {
            Ok(p) => p,
            Err(e) => return fail(&state, tenant, flow_id, &runtime, &e),
        };

        // Emitted BEFORE the wait, which is the whole reason `begin` and `wait`
        // are separate calls: the code has to be on screen while we poll.
        state.registry.publish(
            tenant,
            UiEvent::RuntimeAuthPrompt {
                flow_id,
                runtime: runtime.clone(),
                user_code: pending.user_code.clone(),
                verification_uri: pending.link().to_string(),
                expires_in_secs: pending.expires_in.as_secs(),
            },
        );

        let credential = match flow.wait(&pending).await {
            Ok(c) => c,
            Err(e) => return fail(&state, tenant, flow_id, &runtime, &e),
        };

        deliver(&state, tenant, flow_id, &runtime, &credential, &nodes);
    });
}

/// One terminal failure event, and nothing persisted. The credential either
/// never existed or is dropped here with the flow.
fn fail(state: &AppState, tenant: TenantId, flow_id: Uuid, runtime: &str, e: &RuntimeAuthError) {
    tracing::warn!(%flow_id, %runtime, error = %e, "runtime authorization failed");
    state.registry.publish(
        tenant,
        UiEvent::RuntimeAuthFailed {
            flow_id,
            runtime: runtime.to_string(),
            kind: error_kind(e).to_string(),
            message: e.to_string(),
        },
    );
}

/// Send the credential to every selected node.
///
/// Authorize once, deliver to N. A node that is not connected is reported as a
/// failed delivery immediately rather than left pending — `send_to_node`
/// answering `false` is a definite "this did not happen", and the UI should say
/// so instead of showing a spinner forever.
fn deliver(
    state: &AppState,
    tenant: TenantId,
    flow_id: Uuid,
    runtime: &str,
    credential: &RuntimeCredential,
    nodes: &[NodeId],
) {
    use base64::Engine as _;
    let payload_b64 = base64::engine::general_purpose::STANDARD.encode(&credential.payload);

    for &node_id in nodes {
        // Recorded BEFORE the send: the node can answer faster than this loop
        // continues, and a report arriving before its expectation exists would
        // be dropped as uncorrelated.
        state.pending_deliveries.expect(node_id, runtime, flow_id);

        let sent = state.registry.send_to_node(
            node_id,
            ControlToNode::InstallRuntimeCredential {
                runtime: runtime.to_string(),
                payload_b64: payload_b64.clone(),
            },
        );
        if !sent {
            state.pending_deliveries.forget(node_id, runtime);
            tracing::warn!(%flow_id, %runtime, node = %node_id.0, "node offline — credential not delivered");
            state.registry.publish(
                tenant,
                UiEvent::RuntimeAuthDelivered {
                    flow_id,
                    node_id,
                    runtime: runtime.to_string(),
                    error: Some("the node is not connected".into()),
                },
            );
        }
    }
}

/// How long a flow's expectations are worth keeping. Longer than any device
/// code's lifetime, so a slow-but-successful approval is never orphaned.
pub const DELIVERY_TTL: Duration = Duration::from_secs(30 * 60);

/// Shared handle type, so `AppState` can hold one.
pub type SharedPendingDeliveries = Arc<PendingDeliveries>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_resolves_exactly_one_delivery() {
        let p = PendingDeliveries::new();
        let node = NodeId::new();
        let flow = Uuid::now_v7();
        p.expect(node, "claude", flow);

        assert_eq!(p.take(node, "claude"), Some(flow));
        // Consumed: a second install of the same runtime on the same node is a
        // different event and must not be attributed to a finished flow.
        assert_eq!(p.take(node, "claude"), None);
    }

    #[test]
    fn deliveries_are_keyed_by_node_and_runtime_together() {
        let p = PendingDeliveries::new();
        let (a, b) = (NodeId::new(), NodeId::new());
        let (f1, f2) = (Uuid::now_v7(), Uuid::now_v7());
        p.expect(a, "claude", f1);
        p.expect(b, "claude", f2);
        p.expect(a, "hermes", f2);

        assert_eq!(p.take(a, "claude"), Some(f1), "one node's flow is its own");
        assert_eq!(p.take(b, "claude"), Some(f2));
        assert_eq!(
            p.take(a, "hermes"),
            Some(f2),
            "the same node can await two runtimes at once"
        );
    }

    #[test]
    fn forgetting_an_undelivered_node_leaves_nothing_to_capture_a_later_install() {
        let p = PendingDeliveries::new();
        let node = NodeId::new();
        p.expect(node, "claude", Uuid::now_v7());
        p.forget(node, "claude");
        assert_eq!(
            p.take(node, "claude"),
            None,
            "an offline node's entry must not sit there capturing an unrelated install"
        );
    }

    /// The UI branches on these, so they are part of the contract.
    #[test]
    fn every_error_has_its_own_kind() {
        assert_eq!(error_kind(&RuntimeAuthError::Expired), "expired");
        assert_eq!(error_kind(&RuntimeAuthError::Denied), "denied");
        assert_eq!(
            error_kind(&RuntimeAuthError::Provider(String::new())),
            "provider"
        );
        assert_eq!(
            error_kind(&RuntimeAuthError::Transport(String::new())),
            "transport"
        );
    }
}
