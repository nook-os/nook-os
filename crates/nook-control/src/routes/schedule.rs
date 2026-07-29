use axum::extract::{Query, State};
use axum::Json;
use nook_db::{params, Db};
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
    // The New Work "Auto" picker resolves only nodes the session user owns
    // (MAIN-131), matching where a session may actually start.
    let node_id =
        schedule::pick(&state, auth.tenant_id, Some(auth.user_id), q.workspace_id).await?;
    let node_name = state
        .db
        .query_scalar::<String>("SELECT name FROM nodes WHERE id = $1", params![node_id])
        .await?;
    Ok(Json(ScheduledNode { node_id, node_name }))
}
