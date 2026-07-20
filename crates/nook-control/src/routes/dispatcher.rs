use axum::extract::State;
use axum::Json;
use nook_dispatcher::{DispatchContext, DispatchError};
use nook_types::*;

use crate::auth::AuthCtx;
use crate::error::{ApiError, ApiResult};
use crate::services::core;
use crate::state::AppState;

impl From<DispatchError> for ApiError {
    fn from(e: DispatchError) -> Self {
        match e {
            DispatchError::NotConfigured(id) => {
                ApiError::BadRequest(format!("dispatcher backend '{id}' is not configured"))
            }
            DispatchError::Internal(m) => ApiError::Internal(anyhow::anyhow!(m)),
        }
    }
}

#[utoipa::path(post, path = "/api/v1/dispatcher/suggest",
    operation_id = "dispatcher_suggest",
    responses((status = 200, body = DispatchSuggestion)))]
pub async fn suggest(
    State(state): State<AppState>,
    auth: AuthCtx,
) -> ApiResult<Json<DispatchSuggestion>> {
    // Assemble the dispatcher's (deliberately limited) view of the world.
    let boards = state.kanban.all_boards(auth.tenant_id).await?;
    let mut tasks = Vec::new();
    let mut columns = Vec::new();
    for board in &boards {
        if let Some(provider) = state.kanban.get(&board.provider) {
            if let Ok(detail) = provider.board_detail(auth.tenant_id, board.id).await {
                tasks.extend(detail.tasks);
                columns.extend(detail.columns);
            }
        }
    }
    let sessions = core::list_sessions(&state.db, auth.tenant_id, None, true).await?;
    let nodes = core::list_nodes(&state.db, auth.tenant_id).await?;

    let suggestion = state
        .dispatcher
        .suggest(DispatchContext {
            tasks,
            columns,
            active_sessions: sessions.len(),
            online_nodes: nodes.iter().filter(|n| n.status == "online").count(),
        })
        .await?;
    Ok(Json(suggestion))
}
