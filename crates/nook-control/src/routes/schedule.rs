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
    let node_id = schedule::pick(&state, auth.tenant_id, q.workspace_id).await?;
    let (node_name,): (String,) = sqlx::query_as("SELECT name FROM nodes WHERE id = $1")
        .bind(node_id)
        .fetch_one(&state.db)
        .await?;
    Ok(Json(ScheduledNode { node_id, node_name }))
}
