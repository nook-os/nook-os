//! Authorize a runtime without a session (MAIN-290, C3).
//!
//! The replacement for `POST /nodes/{id}/authorize`, which spawns
//! `claude auth login` in a terminal and needs a person sitting at that
//! terminal. This runs the device flow in the control plane and delivers the
//! resulting credential to every selected machine — authorize once, deliver to
//! N. The old endpoint stays until C5 retires it (NG-1).
//!
//! The response is a **202 with a flow id**, not a result: approval is a human
//! typing a code into somebody else's website, and no HTTP request should be
//! held open for that. Everything after `begin()` reports through `UiEvent`s
//! keyed by the flow id.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use nook_types::NodeId;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::runtime_auth::{descriptor_for, DeviceFlow};
use crate::services::runtime_auth_flow;
use crate::state::AppState;

#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct RuntimeAuthRequest {
    /// The runtime to authorize — `claude` today.
    pub runtime: String,
    /// Every machine that should end up with the credential. One approval
    /// covers all of them.
    pub node_ids: Vec<NodeId>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct RuntimeAuthAccepted {
    /// Correlates every `UiEvent` this flow emits. The caller watches the
    /// socket it already has.
    pub flow_id: Uuid,
    pub runtime: String,
}

/// Start a sessionless authorization.
///
/// Authorization to *run* this is the same rule the session-based endpoint
/// uses, applied to every named node: a personal machine is its owner's alone,
/// a shared or operator machine needs `node.manage`. Delivering a credential to
/// a shared machine makes that runtime available to every workload already
/// permitted to run there, which is not one person's call.
#[utoipa::path(post, path = "/api/v1/runtime-auth",
    operation_id = "start_runtime_auth",
    request_body = RuntimeAuthRequest,
    responses((status = 202, body = RuntimeAuthAccepted), (status = 400), (status = 403), (status = 404)))]
pub async fn start(
    State(state): State<AppState>,
    auth: AuthCtx,
    Json(req): Json<RuntimeAuthRequest>,
) -> ApiResult<(StatusCode, Json<RuntimeAuthAccepted>)> {
    use crate::auth::perm::{Permission, Scope};

    // Same rule as the session-based twin (`nodes::authorize`): the owner leg
    // resolves the caller's person, and a node token carries the tenant
    // owner's (MAIN-577).
    auth.require_user()?;

    // The REQUEST is validated before any node is authorized, deliberately: a
    // 400 about the runtime or an empty list leaks nothing, whereas the node
    // check reveals whether a node exists. Doing the harmless refusal first
    // also keeps the answer independent of which runtimes a deployment happens
    // to have configured.
    if req.node_ids.is_empty() {
        return Err(ApiError::BadRequest(
            "name at least one node to deliver the credential to".into(),
        ));
    }

    // Named rather than generic, so an operator can tell "we cannot authorize
    // that runtime this way" from "something went wrong".
    let Some(descriptor) = descriptor_for(&req.runtime) else {
        return Err(ApiError::BadRequest(format!(
            "no device-flow descriptor for runtime {:?} — this runtime cannot be \
             authorized without a session yet",
            req.runtime
        )));
    };

    // Every node is authorized BEFORE the flow starts. Checking as we deliver
    // would mean a person had already approved a credential we then refuse to
    // hand over — the refusal has to come first.
    for &id in &req.node_ids {
        let Some((sharing, caps)) = state
            .nodes
            .sharing_and_capabilities(id, auth.tenant_id)
            .await?
        else {
            // Unknown/invisible node is a 404, not a 403 — no existence oracle.
            return Err(ApiError::NotFound);
        };
        let is_operator = caps
            .get("shared_operator")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if sharing.shared || is_operator {
            auth.require(
                &state,
                Permission::NodeManage,
                Scope::Tenant(auth.tenant_id),
            )
            .await?;
        } else {
            crate::auth::require_person_owns_node(&state, auth.tenant_id, Some(auth.user_id), id)
                .await?;
        }
    }

    let flow_id = Uuid::now_v7();
    runtime_auth_flow::spawn(
        state.clone(),
        auth.tenant_id,
        flow_id,
        DeviceFlow::new(descriptor),
        req.node_ids,
    );

    crate::events::record(
        &state,
        auth.tenant_id,
        crate::events::EventDraft::new("runtime_auth.started")
            .actor("user", auth.user_id.0)
            .payload(serde_json::json!({ "runtime": req.runtime, "flow_id": flow_id })),
    )
    .await;

    Ok((
        StatusCode::ACCEPTED,
        Json(RuntimeAuthAccepted {
            flow_id,
            runtime: req.runtime,
        }),
    ))
}
