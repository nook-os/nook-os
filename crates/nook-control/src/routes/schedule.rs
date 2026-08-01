use axum::extract::{Query, State};
use axum::Json;
use nook_types::*;
use serde::Deserialize;

use crate::auth::AuthCtx;
use crate::error::ApiResult;
use crate::services::schedule;
use crate::state::AppState;

#[derive(Deserialize, utoipa::IntoParams)]
pub struct ScheduleQuery {
    /// Prefer a node that already hosts this workspace.
    pub workspace_id: Option<WorkspaceId>,
}

/// Resolve the "Auto (best available)" node the UI leaves selected by default.
#[utoipa::path(get, path = "/api/v1/schedule/node",
    operation_id = "schedule_node",
    params(ScheduleQuery),
    responses((status = 200, body = ScheduledNode), (status = 400, description = "no online node")))]
pub async fn node(
    State(state): State<AppState>,
    auth: AuthCtx,
    Query(q): Query<ScheduleQuery>,
) -> ApiResult<Json<ScheduledNode>> {
    // The New Workspace "Auto" picker resolves only nodes the session user owns
    // (MAIN-131), matching where a session may actually start.
    // The picker only needs the chosen node here; needs-clone is surfaced at
    // dispatch time (MAIN-227), not on this pre-selection.
    let node_id = schedule::pick(&state, auth.tenant_id, Some(auth.user_id), q.workspace_id)
        .await?
        .node_id();
    // `pick` just read this node out of `nodes`, so the None arm is unreachable;
    // it stays an internal error rather than a 404 so the status is the one the
    // inline read gave when its row was missing.
    let node_name = state.nodes.name_of(node_id).await?.ok_or_else(|| {
        crate::error::ApiError::Internal(anyhow::anyhow!(
            "placed node {node_id} vanished before it could be named"
        ))
    })?;
    Ok(Json(ScheduledNode { node_id, node_name }))
}
